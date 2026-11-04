# Development cluster

A development cluster spreads your workspace's development site over several machines. Your workspace's stack is the manager, and each worker joins from this machine or from a [remote host](remote-hosts.md). Use it to test deployments on hardware your machine lacks.

`just --list dev::cluster` shows every command.

## Start a cluster

Workers join while the manager is stopped:

```sh
just dev::down
just dev::site-reset simulated docker
just dev::cluster::join gpu:gpu1 -- --name gpu1 --inference nvidia --applications docker --nvidia-device nvidia.com/gpu=0
just dev::cluster::join mac:mac1 -- --name mac1 --inference apple --applications docker --native-workload-lifetime ephemeral
just dev::cluster::up
```

- The arguments after `--` go to `dotc node join`.
- On a Mac, pass `--native-workload-lifetime ephemeral`, so its models stop with the worker.
- Any worker can join with `--inference simulated` to test placement and routing without loading a model. `--inference simulated:<name>` picks the hardware it simulates, as for the site.
- To try it on one machine, `just dev::cluster::two-node-init simulated docker` sets up a manager and one local worker. Its last argument gives the worker its own inference choices, so `just dev::cluster::two-node-init simulated:gb10 docker Mac 127.0.0.2 "" simulated:mac-64g` runs a GB10 manager and a Mac worker with no hardware.

A worker is ready when `just dev::cluster::logs <worker>` shows `Manager accepted the node process observation`. A remote worker's first start compiles the backend, which takes a few minutes.

## Deploy and test

In the console, deploy on a worker by picking its target, shown as `<node> / <target>`.

A remote worker runs the code last sent to its host. After a backend or protocol change, run `just dev::cluster::down`, then `just dev::cluster::up`.

## Stop and clean up

- `just dev::cluster::down` stops every worker, then the manager.
- `just dev::cluster::clean` stops the cluster, makes every worker leave, then deletes the manager site. `clean <worker>` removes one worker only.
- Afterwards, check each remote host for leftover containers: `just dev::remote::exec <host> 'docker ps -a'`.

A worker can fail to leave when an app's data holds files it can't delete, such as Open WebUI's model cache. Deploy apps on a development cluster only when you need them.
