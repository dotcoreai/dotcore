import { ArrowDownIcon, ArrowUpIcon, PlusIcon, XIcon } from "lucide-react";
import { type FormEvent, useRef } from "react";

import { Button } from "@/components/ui/button";
import { Combobox, type ComboboxOption } from "@/components/ui/combobox";
import { FieldError } from "@/components/ui/field-error";
import { Input } from "@/components/ui/input";
import { Notice } from "@/components/ui/notice";
import { useDelayedLoading } from "@/hooks/use-delayed-loading";
import { type FieldErrors, focusFirstInvalidField } from "@/lib/form-validation";

import {
  MAX_ROUTE_GROUPS,
  newEditorGroup,
  type RouteEditorGroup,
  type RouteEditorState,
  validateRouteEditor,
} from "./route-draft";
import { selectionForMemberCount, selectionLabel, tierLabel } from "./route-projection";

/**
 * The one form that creates and edits a route: its name, the Primary tier, and any Fallback tiers.
 * The form validates the draft on submit, shows each error next to its field, focuses the first
 * invalid field, and calls `onSave` only for a valid draft. A server error from the last save
 * renders inside the form, next to the Save button. The route's enabled state is not part of the
 * form: new routes are created enabled, and the route details toggle it.
 */
export function RouteEditor({
  deployments,
  editor,
  errors,
  loading = false,
  onChange,
  onClearError,
  onErrors,
  onSave,
  pending = false,
  serverError,
  takenNames,
}: {
  deployments: readonly ComboboxOption[];
  editor: RouteEditorState;
  errors: FieldErrors;
  loading?: boolean;
  onChange: (editor: RouteEditorState) => void;
  onClearError: (field: string) => void;
  onErrors: (errors: FieldErrors) => void;
  onSave: () => void;
  pending?: boolean;
  serverError?: string;
  takenNames: ReadonlySet<string>;
}) {
  const nameRef = useRef<HTMLInputElement>(null);
  const deploymentRefs = useRef<Map<string, HTMLInputElement | null>>(new Map());
  const loadingMessageVisible = useDelayedLoading(loading);
  const nameError = errors.get("name");
  const groupsError = errors.get("groups");

  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (pending) return;

    const validation = validateRouteEditor(
      editor,
      takenNames,
      (id) => deployments.find((option) => option.id === id)?.label ?? id,
    );
    onErrors(validation);
    if (validation.size) {
      focusFirstInvalidField(
        validation,
        new Map([["name", nameRef.current], ...deploymentRefs.current]),
      );
      return;
    }

    onSave();
  };

  const updateGroups = (groups: RouteEditorGroup[]) => onChange({ ...editor, groups });

  return (
    <form
      aria-label={editor.route ? "Edit route" : "Create route"}
      className="grid gap-4"
      noValidate
      onSubmit={submit}
    >
      <div className="grid gap-1.5">
        <label className="text-sm font-medium" htmlFor="route-name">
          Name
        </label>
        <Input
          aria-describedby={nameError ? "route-name-error" : undefined}
          aria-invalid={nameError ? true : undefined}
          autoComplete="off"
          id="route-name"
          onChange={(event) => {
            onClearError("name");
            onChange({ ...editor, name: event.target.value });
          }}
          ref={nameRef}
          value={editor.name}
        />
        {nameError ? <FieldError id="route-name-error">{nameError}</FieldError> : null}
      </div>
      <div>
        {editor.groups.map((group, index) => (
          <RouteEditorTier
            deployments={deployments}
            error={errors.get(`group-${index}-deployment`)}
            group={group}
            groupCount={editor.groups.length}
            index={index}
            inputRef={(control) => {
              deploymentRefs.current.set(`group-${index}-deployment`, control);
            }}
            key={group.key}
            loading={loading || loadingMessageVisible}
            loadingMessageVisible={loadingMessageVisible}
            onMembersChange={(deploymentIds) => {
              onClearError(`group-${index}-deployment`);
              updateGroups(
                editor.groups.map((current, currentIndex) =>
                  currentIndex === index ? { ...current, deploymentIds } : current,
                ),
              );
            }}
            onMove={(offset) => updateGroups(moveGroup(editor.groups, index, offset))}
            onRemove={() => {
              onClearError("groups");
              updateGroups(editor.groups.filter((_, current) => current !== index));
            }}
          />
        ))}
      </div>
      <div className="grid gap-1.5">
        <Button
          className="w-fit"
          disabled={editor.groups.length >= MAX_ROUTE_GROUPS}
          onClick={() => updateGroups([...editor.groups, newEditorGroup()])}
          size="console"
          type="button"
          variant="outline"
        >
          <PlusIcon aria-hidden="true" data-icon="inline-start" />
          Add fallback
        </Button>
        {groupsError ? <FieldError id="route-groups-error">{groupsError}</FieldError> : null}
      </div>
      {serverError ? (
        <Notice role="alert" tone="destructive">
          {serverError}
        </Notice>
      ) : null}
      <Button className="w-fit" disabled={pending} size="console" type="submit">
        {pending ? "Saving…" : editor.route ? "Save" : "Save route"}
      </Button>
    </form>
  );
}

/**
 * One tier of the editor: its tier name and mode, compact move and remove controls, and the
 * deployment picker for its members. Tiers after the first hang from a thin connector, matching the
 * route inspector's flow.
 */
function RouteEditorTier({
  deployments,
  error,
  group,
  groupCount,
  index,
  inputRef,
  loading,
  loadingMessageVisible,
  onMembersChange,
  onMove,
  onRemove,
}: {
  deployments: readonly ComboboxOption[];
  error?: string;
  group: RouteEditorGroup;
  groupCount: number;
  index: number;
  inputRef: (control: HTMLInputElement | null) => void;
  loading: boolean;
  loadingMessageVisible: boolean;
  onMembersChange: (deploymentIds: string[]) => void;
  onMove: (offset: -1 | 1) => void;
  onRemove: () => void;
}) {
  // Visible labels carry no numbers; controls still need a name that tells two fallbacks apart.
  const name = index === 0 ? "Primary" : `Fallback ${index}`;
  const headingId = `route-tier-${group.key}-heading`;
  const errorId = `route-tier-${group.key}-error`;
  const memberCount = group.deploymentIds.length;

  return (
    <>
      {index > 0 ? <div aria-hidden="true" className="ml-5 h-3 w-px bg-border" /> : null}
      <div
        aria-labelledby={headingId}
        className="overflow-hidden rounded-lg border border-border bg-card"
        role="group"
      >
        <div className="flex min-h-10 items-center justify-between gap-2 border-b border-border py-1 pr-1.5 pl-3">
          <span className="text-sm font-semibold" id={headingId}>
            {tierLabel(index)}
            {memberCount > 0 ? (
              <>
                {" "}
                <span className="text-xs font-normal text-muted-foreground">
                  · {selectionLabel(selectionForMemberCount(memberCount))}
                </span>
              </>
            ) : null}
          </span>
          <div className="flex gap-0.5 pointer-coarse:gap-2">
            <Button
              aria-label={`Move ${name} up`}
              disabled={index === 0}
              onClick={() => onMove(-1)}
              size="icon-sm"
              type="button"
              variant="ghost"
            >
              <ArrowUpIcon aria-hidden="true" />
            </Button>
            <Button
              aria-label={`Move ${name} down`}
              disabled={index === groupCount - 1}
              onClick={() => onMove(1)}
              size="icon-sm"
              type="button"
              variant="ghost"
            >
              <ArrowDownIcon aria-hidden="true" />
            </Button>
            <Button
              aria-label={`Remove ${name}`}
              disabled={groupCount <= 1}
              onClick={onRemove}
              size="icon-sm"
              type="button"
              variant="ghost"
            >
              <XIcon aria-hidden="true" />
            </Button>
          </div>
        </div>
        <div className="grid gap-1.5 p-3">
          <Combobox
            ariaInvalid={Boolean(error)}
            closeOnSelect
            errorDescriptionId={error ? errorId : undefined}
            hideLabel
            inputRef={inputRef}
            label={`${name} deployments`}
            loading={loading}
            loadingMessageVisible={loadingMessageVisible}
            multiple
            onValueChange={onMembersChange}
            options={deployments}
            placeholder="Choose deployments"
            value={group.deploymentIds}
          />
          {error ? <FieldError id={errorId}>{error}</FieldError> : null}
        </div>
      </div>
    </>
  );
}

function moveGroup(groups: RouteEditorGroup[], index: number, offset: -1 | 1): RouteEditorGroup[] {
  const next = index + offset;
  if (next < 0 || next >= groups.length) return groups;

  const moved = [...groups];
  [moved[index], moved[next]] = [moved[next], moved[index]];
  return moved;
}
