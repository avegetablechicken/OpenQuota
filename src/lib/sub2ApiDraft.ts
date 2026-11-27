/**
 * The Sub2API connection editor as plain data: one draft per Track, what switching Track,
 * upstream, provider or address does to it, whether a saved secret is kept, and what a save
 * submits. The view keeps backend calls, provider lookups, focus and messages.
 *
 * Every function returns a new value and leaves its arguments unchanged.
 */
import { sub2ApiResolvesProviderNames } from './sub2ApiUpstreams';
import type {
  ReusableCredential,
  Sub2ApiConfigInput,
  Sub2ApiConfigState,
  Sub2ApiMode,
  Sub2ApiUpstream,
} from './types';

/** What the editor holds for one Track. */
export interface ConnectionDraft {
  /** The client configuration a provider name is read from. */
  providerSource: Sub2ApiUpstream;
  /** The user asked to stop using the saved API key. */
  clearSavedKey: boolean;
  baseUrl: string;
  providerName: string;
  customBaseUrl: boolean;
  email: string;
  password: string;
  apiKey: string;
  revealPassword: boolean;
}

export interface Editor {
  mode: Sub2ApiMode;
  upstream: Sub2ApiUpstream;
  /** The Track being edited. */
  draft: ConnectionDraft;
  /** Drafts kept for the other Track while it is not shown. */
  drafts: Partial<Record<Sub2ApiMode, ConnectionDraft>>;
}

/**
 * What the view does about the provider lookup after a change: leave it running, stop it, or
 * look up `name` (cancelling any lookup in flight).
 */
export type ProviderLookup =
  { kind: 'keep' } | { kind: 'stop' } | { kind: 'resolve'; name: string };

export interface EditorChange {
  editor: Editor;
  lookup: ProviderLookup;
}

export interface Review {
  /** A saved secret for this Track matches the draft, so none needs entering. */
  canReuseCredential: boolean;
  /** The saved API key will be kept. */
  keepSavedKey: boolean;
  /** The subscription reads its key from the provider's configuration. */
  automaticKey: boolean;
  canSave: boolean;
}

/** The state of the provider lookup the draft's Base URL comes from. */
export interface LookupState {
  resolving: boolean;
  failed: boolean;
}

export function emptyDraft(): ConnectionDraft {
  return {
    providerSource: 'codex',
    baseUrl: '',
    providerName: '',
    clearSavedKey: false,
    customBaseUrl: false,
    email: '',
    password: '',
    apiKey: '',
    revealPassword: false,
  };
}

/** The Track a saved state is on. */
export function savedMode(state: Sub2ApiConfigState): Sub2ApiMode {
  return state.mode ?? (state.configured ? 'admin' : 'subscription');
}

function shown(draft: ConnectionDraft, upstream: Sub2ApiUpstream): ConnectionDraft {
  const providerSource = draft.providerName.trim() ? draft.providerSource : upstream;
  return {
    ...draft,
    providerSource,
    customBaseUrl: draft.customBaseUrl || !sub2ApiResolvesProviderNames(providerSource),
  };
}

/** An editor showing `state`'s saved Track, with a draft for each saved Track. */
export function editorFor(state: Sub2ApiConfigState): Editor {
  const mode = savedMode(state);
  const drafts: Partial<Record<Sub2ApiMode, ConnectionDraft>> = {};
  for (const profile of [state, state.otherMode]) {
    if (!profile) continue;
    drafts[profile.mode ?? mode] = {
      ...emptyDraft(),
      providerSource: profile.providerSource ?? profile.upstream,
      baseUrl: profile.baseUrl,
      providerName: profile.codexProvider ?? '',
      customBaseUrl: profile.customBaseUrl,
      email: profile.email,
    };
  }
  return { mode, upstream: state.upstream, draft: shown(drafts[mode]!, state.upstream), drafts };
}

function lookupFor(editor: Editor): ProviderLookup {
  const name = editor.draft.providerName.trim();
  return !name || editor.draft.customBaseUrl ? { kind: 'stop' } : { kind: 'resolve', name };
}

/** Typing a provider name clears the address until the name is looked up. */
export function withProviderName(editor: Editor, value: string): EditorChange {
  const { draft, upstream } = editor;
  const followUpstream =
    (!value.trim() || !draft.providerName.trim()) && sub2ApiResolvesProviderNames(upstream);
  const next: Editor = {
    ...editor,
    draft: {
      ...draft,
      providerSource: followUpstream ? upstream : draft.providerSource,
      providerName: value,
      baseUrl: '',
    },
  };
  return { editor: next, lookup: lookupFor(next) };
}

/** Switches Track, keeping the current Track's draft for when it is shown again. */
export function withMode(editor: Editor, mode: Sub2ApiMode): EditorChange {
  const drafts = { ...editor.drafts, [editor.mode]: editor.draft };
  const next: Editor = {
    ...editor,
    mode,
    drafts,
    draft: shown(drafts[mode] ?? emptyDraft(), editor.upstream),
  };
  if (!next.draft.customBaseUrl && next.draft.providerName.trim()) {
    return withProviderName(next, next.draft.providerName);
  }
  return { editor: next, lookup: { kind: 'stop' } };
}

/**
 * Marks the connection as tracking `upstream`. An administrator's named provider stays as it is;
 * otherwise the address follows the upstream, entered by hand when it names no providers.
 */
export function withUpstream(editor: Editor, upstream: Sub2ApiUpstream): EditorChange {
  const { draft } = editor;
  if (editor.mode === 'admin' && draft.providerName.trim()) {
    return { editor: { ...editor, upstream }, lookup: { kind: 'keep' } };
  }
  if (!sub2ApiResolvesProviderNames(upstream)) {
    // Keep the address already shown: upstreams of one instance share it.
    return {
      editor: {
        ...editor,
        upstream,
        draft: { ...draft, providerSource: upstream, customBaseUrl: true, providerName: '' },
      },
      lookup: { kind: 'stop' },
    };
  }
  const next: Editor = { ...editor, upstream, draft: { ...draft, providerSource: upstream } };
  if (!next.draft.customBaseUrl && next.draft.providerName.trim()) {
    return withProviderName(next, next.draft.providerName);
  }
  return { editor: next, lookup: { kind: 'keep' } };
}

/** Switches between a named provider and an entered Base URL, starting the address over. */
export function withCustomBaseUrl(editor: Editor, customBaseUrl: boolean): EditorChange {
  return {
    editor: {
      ...editor,
      draft: {
        ...editor.draft,
        customBaseUrl,
        providerSource: editor.upstream,
        baseUrl: '',
        providerName: '',
      },
    },
    lookup: { kind: 'stop' },
  };
}

/** Whether `draft` matches every field `scope` names. */
export function credentialAdmits(scope: ReusableCredential, draft: ConnectionDraft): boolean {
  return (
    draft.baseUrl.trim().replace(/\/+$/, '') === scope.baseUrl &&
    (scope.customBaseUrl === undefined || scope.customBaseUrl === draft.customBaseUrl) &&
    (scope.providerSource === undefined || scope.providerSource === draft.providerSource) &&
    (scope.codexProvider === undefined || scope.codexProvider === draft.providerName.trim())
  );
}

function credentialChoice(editor: Editor, state: Sub2ApiConfigState) {
  const { mode, draft } = editor;
  const saved = savedMode(state);
  const profile = mode === saved ? state : state.otherMode;
  const scope =
    profile?.configured && (profile.mode ?? saved) === mode
      ? profile.reusableCredential
      : undefined;
  const canReuseCredential = Boolean(scope && credentialAdmits(scope, draft));
  const keepSavedKey = canReuseCredential && !draft.clearSavedKey;
  const automaticKey =
    mode === 'subscription' && !draft.customBaseUrl && !draft.apiKey.trim() && !keepSavedKey;
  return { canReuseCredential, keepSavedKey, automaticKey };
}

/** What the editor promises about saved secrets, and whether it can save now. */
export function review(
  editor: Editor,
  state: Sub2ApiConfigState,
  { lookup, saving }: { lookup: LookupState; saving: boolean },
): Review {
  const { mode, draft } = editor;
  const choice = credentialChoice(editor, state);
  const endpointReady = !draft.customBaseUrl
    ? Boolean(
        draft.providerName.trim() && draft.baseUrl.trim() && !lookup.failed && !lookup.resolving,
      )
    : Boolean(draft.baseUrl.trim());
  const credentialReady =
    mode === 'subscription'
      ? !draft.customBaseUrl || choice.keepSavedKey || Boolean(draft.apiKey.trim())
      : choice.canReuseCredential || Boolean(draft.password.trim());
  return {
    ...choice,
    canSave:
      endpointReady &&
      (mode === 'subscription' || Boolean(draft.email.trim())) &&
      credentialReady &&
      !saving,
  };
}

/**
 * What a save sends, and the state to show until the backend answers. The optimistic state names
 * no reusable secret; the backend's answer does.
 */
export function submission(
  editor: Editor,
  state: Sub2ApiConfigState,
): { input: Sub2ApiConfigInput; optimistic: Sub2ApiConfigState } {
  const { mode, upstream, draft } = editor;
  const { automaticKey } = credentialChoice(editor, state);
  const input: Sub2ApiConfigInput = {
    baseUrl: draft.baseUrl.trim(),
    codexProvider: draft.providerName.trim(),
    ...(!draft.customBaseUrl && draft.providerSource !== upstream
      ? { providerSource: draft.providerSource }
      : {}),
    customBaseUrl: draft.customBaseUrl,
    mode,
    email: mode === 'admin' ? draft.email.trim() : '',
    password: mode === 'admin' ? draft.password : '',
    ...(automaticKey
      ? { useProviderKey: true }
      : mode === 'subscription'
        ? { apiKey: draft.apiKey.trim() }
        : {}),
    upstream,
  };
  const optimistic: Sub2ApiConfigState = {
    mode: input.mode,
    useProviderKey: automaticKey,
    otherMode:
      input.mode === savedMode(state)
        ? state.otherMode
        : state.configured
          ? { ...state, otherMode: undefined }
          : undefined,
    configured: true,
    baseUrl: input.baseUrl,
    codexProvider: input.codexProvider,
    customBaseUrl: input.customBaseUrl,
    email: input.email,
    upstream: input.upstream,
    providerSource: input.providerSource ?? input.upstream,
  };
  return { input, optimistic };
}

/**
 * The editor after a failed save: `previous` is shown again with the submitted fields restored,
 * except the secrets.
 */
export function restoreAfterFailure(
  previous: Sub2ApiConfigState,
  input: Sub2ApiConfigInput,
): Editor {
  const editor = editorFor(previous);
  return {
    ...editor,
    mode: input.mode,
    upstream: input.upstream,
    draft: {
      ...editor.draft,
      baseUrl: input.baseUrl,
      providerName: input.codexProvider,
      customBaseUrl: input.customBaseUrl,
      clearSavedKey: Boolean(input.useProviderKey),
      email: input.email,
      providerSource: input.providerSource ?? input.upstream,
      password: '',
      revealPassword: false,
    },
  };
}
