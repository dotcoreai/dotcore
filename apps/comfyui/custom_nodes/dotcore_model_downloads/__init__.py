"""Server-side model downloads for the editor's missing-models dialog.

The dialog hands each missing model's URL, file name and model folder to the editor script of
this node, which asks these routes to download it into the deployment's own model folders
instead of onto the viewing computer. Only the sources the dialog itself allows are accepted,
only into model folders, and an existing file is never replaced. Public models only: a source
asking for a login is refused with that reason.
"""

import asyncio
import errno
import glob
import ipaddress
import os
import shutil
import tempfile
import time
from urllib.parse import urljoin, urlsplit

import aiohttp
import folder_paths
from aiohttp import web
from server import PromptServer

# Mirrors the source list of the pinned frontend's missing-models dialog; the image workflow
# compares both, so a frontend bump that changes its list fails until this one follows.
ALLOWED_SOURCES = (
    "https://civitai.com/",
    "https://civitai.red/",
    "https://huggingface.co/",
)
# Exact URLs the dialog also allows, without its development server's.
ALLOWED_URLS = (
    "https://huggingface.co/stabilityai/stable-zero123/resolve/main/stable_zero123.ckpt",
    "https://huggingface.co/TencentARC/T2I-Adapter/resolve/main/models/t2iadapter_depth_sd14v1.pth?download=true",
    "https://github.com/xinntao/Real-ESRGAN/releases/download/v0.1.0/RealESRGAN_x4plus.pth",
)
ALLOWED_SUFFIXES = (
    ".safetensors",
    ".sft",
    ".ckpt",
    ".pth",
    ".pt",
)

EVENT = "dotcore-model-download"
WRITE_SIZE = 4 << 20
PROGRESS_INTERVAL_SECONDS = 1.0
TIMEOUT = aiohttp.ClientTimeout(total=None, sock_connect=30, sock_read=120)
MAX_REDIRECTS = 10
TEMPORARY_SUFFIX = ".part"
# Answers that are a page or an API reply rather than a file.
NON_MODEL_TYPES = ("text/", "application/json")

WEB_DIRECTORY = "./web"
NODE_CLASS_MAPPINGS = {}

downloads = {}
# Running downloads, held so the event loop does not drop their tasks.
tasks = set()


class PublicResolver(aiohttp.ThreadedResolver):
    """Resolves only to public addresses, so no redirect reaches the deployment's own network."""

    async def resolve(self, host, port=0, family=0):
        results = await super().resolve(host, port, family)
        for result in results:
            if not ipaddress.ip_address(result["host"]).is_global:
                raise OSError(f"{host} resolves to the non-public address {result['host']}")
        return results


class Refusal(Exception):
    """A download refused before any byte is written, with the HTTP status that reports it."""

    def __init__(self, status, reason):
        super().__init__(reason)
        self.status = status


@PromptServer.instance.routes.get("/dotcore/model-downloads")
async def list_downloads(request):
    return web.json_response({"downloads": list(downloads.values())})


@PromptServer.instance.routes.post("/dotcore/model-downloads")
async def start_download(request):
    # A browser sends a cross-site form post without asking first; JSON needs the editor's origin.
    if request.content_type != "application/json":
        return web.json_response({"error": "the request must be JSON"}, status=415)
    try:
        body = await request.json()
        url, name, directory = (str(body.get(key, "")) for key in ("url", "name", "directory"))
    except (ValueError, AttributeError):
        return web.json_response(
            {"error": "the request must be a JSON object with url, name and directory"}, status=400
        )

    download_id = f"{directory}/{name}"
    previous = downloads.get(download_id)
    if previous is not None and previous["state"] == "downloading":
        return web.json_response(
            {"error": f"{download_id} is already downloading", "download": previous}, status=409
        )

    # Registered before the source answers, so a second request for the same file is refused.
    download = {
        "id": download_id,
        "url": url,
        "directory": directory,
        "name": name,
        "state": "downloading",
        "received": 0,
        "total": None,
        "error": None,
    }
    downloads[download_id] = download
    session = None
    try:
        folder, target = resolve_target(url, name, directory)
        session = aiohttp.ClientSession(
            timeout=TIMEOUT, connector=aiohttp.TCPConnector(resolver=PublicResolver())
        )
        response = await open_source(session, url, folder, name)
    except BaseException as error:
        if previous is None:
            del downloads[download_id]
        else:
            downloads[download_id] = previous
        if session is not None:
            await session.close()
        if isinstance(error, Refusal):
            return web.json_response({"error": str(error)}, status=error.status)
        if isinstance(error, OSError):
            return web.json_response(
                {"error": f"the storage refused {download_id}: {error}"},
                status=storage_status(error),
            )
        raise

    download["total"] = response.content_length
    task = asyncio.create_task(run_download(download, session, response, folder, target))
    tasks.add(task)
    task.add_done_callback(tasks.discard)
    return web.json_response({"download": download}, status=202)


def resolve_target(url, name, directory):
    """Validates a request against the allowed sources and model folders, and returns the folder
    and final path the file lands at."""
    if url not in ALLOWED_URLS and not url.startswith(ALLOWED_SOURCES):
        raise Refusal(
            400,
            f"{url} is not from an allowed source: downloads come only from {', '.join(ALLOWED_SOURCES)}",
        )

    folder = model_folder(directory)
    if folder is None:
        raise Refusal(400, f"{directory} is not a model folder")

    if (
        not name
        or name != os.path.basename(name)
        or "\\" in name
        or "\0" in name
        or name.startswith(".")
    ):
        raise Refusal(
            400, f"{name} is not a plain file name: it must not name a folder or start with a dot"
        )
    if not name.endswith(ALLOWED_SUFFIXES):
        raise Refusal(
            400, f"{name} is not a model file: the name must end in {', '.join(ALLOWED_SUFFIXES)}"
        )

    target = os.path.join(folder, name)
    if not folder_paths.is_within_directory(folder, target):
        raise Refusal(400, f"{name} resolves outside the {directory} folder")
    if os.path.lexists(target):
        raise Refusal(
            409,
            f"{directory}/{name} already exists on the storage; delete it there to download it again",
        )
    return folder, target


def model_folders():
    """Returns every model folder path under the models directory."""
    return {
        folder
        for key in folder_paths.folder_names_and_paths
        for folder in folder_paths.get_folder_paths(key)
        if folder_paths.is_within_directory(folder_paths.models_dir, folder)
    }


def model_folder(directory):
    """Returns the path of a model folder key under the models directory: the one named like the
    requested folder, else the key's first. Keys such as custom nodes or datasets have no path
    there, so they never receive a download."""
    key = folder_paths.map_legacy(directory)
    if key not in folder_paths.folder_names_and_paths:
        return None
    folders = [
        folder
        for folder in folder_paths.get_folder_paths(key)
        if folder_paths.is_within_directory(folder_paths.models_dir, folder)
    ]
    # A key can list a legacy folder first, as diffusion models list unet.
    named = [folder for folder in folders if os.path.basename(folder) == directory]
    return (named or folders or [None])[0]


async def open_source(session, url, folder, name):
    """Requests the source, following redirects only over HTTPS, and refuses answers that are no
    model file, or one larger than the storage has room for."""
    for _ in range(MAX_REDIRECTS + 1):
        host = urlsplit(url).hostname or ""
        if urlsplit(url).scheme != "https":
            raise Refusal(502, f"the download of {name} redirects to {url}, which is not HTTPS")
        # Address literals skip the resolver, so they are checked here.
        if not is_public_host(host):
            raise Refusal(502, f"the download of {name} redirects to the non-public address {host}")
        try:
            # Uncompressed, so the announced length is the size of the file on the storage.
            response = await session.get(
                url, headers={"Accept-Encoding": "identity"}, allow_redirects=False
            )
        except (aiohttp.ClientError, asyncio.TimeoutError) as error:
            raise Refusal(
                502, f"{host} could not be reached: {error or type(error).__name__}"
            ) from None
        if response.status not in (301, 302, 303, 307, 308):
            break
        response.release()
        url = urljoin(url, response.headers.get("Location", ""))
    else:
        raise Refusal(502, f"the download of {name} redirects more than {MAX_REDIRECTS} times")

    if response.status in (401, 403):
        response.release()
        raise Refusal(
            502,
            f"{host} answered {response.status}: the model needs a login, which server-side downloads do not support; download it elsewhere and place it on the storage by hand",
        )
    if response.status != 200:
        response.release()
        raise Refusal(502, f"{host} answered {response.status} {response.reason} for {name}")
    if response.content_type == "text/html":
        response.release()
        raise Refusal(
            502,
            f"{host} answered with a web page instead of {name}; the model likely needs a login, which server-side downloads do not support",
        )
    if response.content_type.startswith(NON_MODEL_TYPES) or response.content_length == 0:
        response.release()
        answer = response.content_type if response.content_length != 0 else "an empty file"
        raise Refusal(502, f"{host} answered with {answer} instead of a model file")

    try:
        os.makedirs(folder, exist_ok=True)
        free = shutil.disk_usage(folder).free
    except OSError as error:
        response.release()
        raise Refusal(
            storage_status(error), f"the storage refused the {name} folder: {error}"
        ) from None
    # Running downloads still need their remaining bytes from the same free space.
    reserved = sum(
        download["total"] - download["received"]
        for download in downloads.values()
        if download["state"] == "downloading" and download["total"] is not None
    )
    if response.content_length is not None and response.content_length > free - reserved:
        response.release()
        raise Refusal(
            507,
            f"{name} needs {response.content_length / 2**30:.1f} GiB, the storage has {max(free - reserved, 0) / 2**30:.1f} GiB free after running downloads",
        )
    return response


async def run_download(download, session, response, folder, target):
    """Streams the answer into an exclusive temporary file, written off the event loop, and links
    it to its final name only when complete, which fails rather than replaces an existing file."""
    temporary = None
    try:
        handle, temporary = tempfile.mkstemp(
            dir=folder, prefix=f".{download['name']}.", suffix=TEMPORARY_SUFFIX
        )
        # The temporary file is private; the model is readable like the rest of the storage.
        os.fchmod(handle, 0o644)
        with os.fdopen(handle, "wb") as file:
            buffer = bytearray()
            reported = time.monotonic()
            async for chunk in response.content.iter_chunked(1 << 20):
                buffer += chunk
                download["received"] += len(chunk)
                if len(buffer) >= WRITE_SIZE:
                    await asyncio.to_thread(file.write, buffer)
                    buffer = bytearray()
                if time.monotonic() - reported >= PROGRESS_INTERVAL_SECONDS:
                    reported = time.monotonic()
                    await PromptServer.instance.send(EVENT, download)
            await asyncio.to_thread(file.write, buffer)
            await asyncio.to_thread(os.fsync, file.fileno())

        if download["total"] is not None and download["received"] != download["total"]:
            raise ValueError(
                f"the source announced {download['total']} bytes but sent {download['received']}"
            )
        try:
            os.link(temporary, target)
        except FileExistsError:
            raise ValueError(
                f"{download['id']} appeared on the storage during the download; it was left as is"
            ) from None
        download["state"] = "done"
    except Exception as error:
        download["state"] = "failed"
        download["error"] = f"download of {download['id']} failed: {error or type(error).__name__}"
    finally:
        if temporary is not None:
            # A failed multi-gigabyte file takes a while to unlink.
            await asyncio.to_thread(remove_file, temporary)
        response.release()
        await session.close()
    await PromptServer.instance.send(EVENT, download)


def is_public_host(host):
    """Whether a URL host is a name, left to the resolver, or a public address literal."""
    try:
        return ipaddress.ip_address(host.strip("[]")).is_global
    except ValueError:
        return True


def storage_status(error):
    """The HTTP status reporting a storage error: insufficient storage when it is full."""
    return 507 if error.errno in (errno.ENOSPC, errno.EDQUOT) else 500


def remove_file(path):
    try:
        os.unlink(path)
    except FileNotFoundError:
        pass


def remove_leftovers():
    """Removes temporary files of downloads a previous process left unfinished; none runs yet."""
    for folder in model_folders():
        for leftover in glob.glob(os.path.join(glob.escape(folder), f".*{TEMPORARY_SUFFIX}")):
            remove_file(leftover)


remove_leftovers()
