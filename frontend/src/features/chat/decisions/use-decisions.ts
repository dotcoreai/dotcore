import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import type { DecisionClient, DecisionResult } from "@/api/decision-client";
import {
  askedMatches,
  bodyToForm,
  type DecisionAskRequest,
  type DecisionDraft,
  type DecisionForm,
  formToBody,
  requestForDraft,
} from "@/features/chat/decisions/decision-draft";
import { decisionExamples } from "@/features/chat/decisions/decision-examples";
import type { DecisionSet, DecisionStore } from "@/features/chat/decisions/decision-store";

/** One Ask: the route and request it was sent with, and its outcome once it arrives. */
export interface DecisionRun {
  route: string;
  request: DecisionAskRequest;
  /** Absent while the request is in flight. */
  result?: DecisionResult;
}

export interface Decisions {
  draft: DecisionDraft;
  /** Why the draft can't build a request yet; Ask stays disabled while it is set. */
  problem?: string;
  /** In the JSON view, why the form can't show the body; the draft stays in JSON while set. */
  formReason?: string;
  /** The last Ask on the route, whose answers are drawn from its own request. */
  run?: DecisionRun;
  /**
   * Edits the form; renaming, adding, removing, reordering or retyping questions clears the
   * answers. In the JSON view the body is rewritten from the edited form.
   */
  updateForm: (change: (form: DecisionForm) => DecisionForm) => void;
  /** Replaces the form, such as with an example, and clears the answers. */
  loadForm: (form: DecisionForm) => void;
  /**
   * Edits the JSON view's body. While the form can show it, the form follows, and the answers
   * clear on the same question changes as a form edit.
   */
  editJson: (text: string) => void;
  /**
   * Switches between the form and the JSON view, keeping the request: the JSON view starts from
   * the form's body, and the form takes the body back unless the form can't show it.
   */
  switchView: (view: DecisionDraft["view"]) => void;
  /** Sends the draft's request to the route, replacing the last Ask. */
  ask: () => void;
  /** Sends the last Ask's request again. */
  retry: () => void;
  /** Drops the last Ask, stopping it when in flight. */
  clear: () => void;
  /** The named question sets saved in the browser, as last read. */
  sets: readonly DecisionSet[];
  /** Reads the saved sets again, picking up sets saved or deleted from another tab. */
  refreshSets: () => void;
  /** Saves the draft's request body under a name, replacing a set of that name. */
  saveSet: (name: string) => void;
  /** Deletes the saved set with this name. */
  deleteSet: (name: string) => void;
  /**
   * Replaces the draft with a saved set's body and clears the answers. A body the form can't show
   * opens in the JSON view.
   */
  loadSet: (body: string) => void;
}

/**
 * Decision state for the console chat: the operator's draft and the last Ask on the selected
 * route. `route` is the inference route name, sent as the model. Changing the route, a new Ask or
 * clearing stops the request in flight and drops its outcome, so answers always belong to the
 * route and request they came from. The draft and the named sets are kept in `store`, so they
 * survive a reload.
 */
export function useDecisions(
  client: DecisionClient,
  route: string | undefined,
  store: DecisionStore,
): Decisions {
  const [draft, setDraft] = useState<DecisionDraft>(() => store.readDraft() ?? initialDraft);
  const [sets, setSets] = useState<readonly DecisionSet[]>(() => store.readSets());
  const [run, setRun] = useState<DecisionRun>();
  const draftRef = useRef(draft);
  const controllerRef = useRef<AbortController | undefined>(undefined);

  // An Ask on another route is dropped before it shows, however it arrives; the effect below
  // stops it when in flight.
  if (run && run.route !== route) setRun(undefined);

  useEffect(
    () => () => {
      controllerRef.current?.abort();
      controllerRef.current = undefined;
    },
    [route],
  );

  const send = useCallback(
    async (request: DecisionAskRequest) => {
      if (!route) return;

      controllerRef.current?.abort();
      const controller = new AbortController();
      controllerRef.current = controller;
      setRun({ route, request });

      const result = await client.ask({
        model: route,
        body: request.body,
        signal: controller.signal,
      });
      // A route change, a newer Ask or clearing replaced this one: its outcome is stale.
      if (controllerRef.current !== controller || !result) return;
      controllerRef.current = undefined;
      setRun({ route, request, result });
    },
    [client, route],
  );

  const clear = useCallback(() => {
    controllerRef.current?.abort();
    controllerRef.current = undefined;
    setRun(undefined);
  }, []);

  const commitDraft = useCallback(
    (next: DecisionDraft) => {
      draftRef.current = next;
      setDraft(next);
      store.writeDraft(next);
    },
    [store],
  );

  const updateForm = useCallback(
    (change: (form: DecisionForm) => DecisionForm) => {
      const current = draftRef.current;
      const form = change(current.form);
      if (run && !askedMatches(run.request.questions, form)) clear();
      commitDraft(withForm(current, form));
    },
    [clear, commitDraft, run],
  );

  const loadForm = useCallback(
    (form: DecisionForm) => {
      clear();
      commitDraft(withForm(draftRef.current, form));
    },
    [clear, commitDraft],
  );

  const editJson = useCallback(
    (jsonText: string) => {
      const current = draftRef.current;
      const form = bodyToForm(jsonText);
      if ("problem" in form) {
        commitDraft({ ...current, jsonText });
        return;
      }
      if (run && !askedMatches(run.request.questions, form)) clear();
      commitDraft({ ...current, form, jsonText });
    },
    [clear, commitDraft, run],
  );

  const switchView = useCallback(
    (view: DecisionDraft["view"]) => {
      const current = draftRef.current;
      if (view === current.view) return;
      if (view === "json") {
        commitDraft({ ...current, view, jsonText: formToBody(current.form) });
        return;
      }

      const form = bodyToForm(current.jsonText ?? "");
      if ("problem" in form) return;
      // The last Ask may come from a body that asked other questions than the form now shows.
      if (run && !askedMatches(run.request.questions, form)) clear();
      commitDraft({ view, form });
    },
    [clear, commitDraft, run],
  );

  const ask = useCallback(() => {
    const request = requestForDraft(draftRef.current);
    if (!("problem" in request)) void send(request);
  }, [send]);

  const retry = useCallback(() => {
    if (run) void send(run.request);
  }, [run, send]);

  const refreshSets = useCallback(() => setSets(store.readSets()), [store]);

  const saveSet = useCallback(
    (name: string) => {
      const request = requestForDraft(draftRef.current);
      if (!("problem" in request)) setSets(store.saveSet({ name, body: request.body }));
    },
    [store],
  );

  const deleteSet = useCallback((name: string) => setSets(store.deleteSet(name)), [store]);

  const loadSet = useCallback(
    (body: string) => {
      const form = bodyToForm(body);
      if (!("problem" in form)) {
        loadForm(form);
        return;
      }
      clear();
      commitDraft({ view: "json", form: draftRef.current.form, jsonText: body });
    },
    [clear, commitDraft, loadForm],
  );

  const built = useMemo(() => requestForDraft(draft), [draft]);
  const problem = "problem" in built ? built.problem : undefined;
  const formReason = useMemo(() => {
    if (draft.view !== "json") return undefined;
    const form = bodyToForm(draft.jsonText ?? "");
    return "problem" in form ? form.problem : undefined;
  }, [draft]);

  return useMemo(
    () => ({
      ask,
      clear,
      deleteSet,
      draft,
      editJson,
      formReason,
      loadForm,
      loadSet,
      problem,
      refreshSets,
      retry,
      run,
      saveSet,
      sets,
      switchView,
      updateForm,
    }),
    [
      ask,
      clear,
      deleteSet,
      draft,
      editJson,
      formReason,
      loadForm,
      loadSet,
      problem,
      refreshSets,
      retry,
      run,
      saveSet,
      sets,
      switchView,
      updateForm,
    ],
  );
}

/** The draft with a new form; the JSON view's body is rewritten from it. */
function withForm(draft: DecisionDraft, form: DecisionForm): DecisionDraft {
  return draft.view === "json"
    ? { ...draft, form, jsonText: formToBody(form) }
    : { ...draft, form };
}

const initialDraft: DecisionDraft = { view: "form", form: decisionExamples[0].form };
