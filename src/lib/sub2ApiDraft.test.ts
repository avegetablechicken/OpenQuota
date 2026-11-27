import { describe, expect, it } from 'vitest';
import {
  editorFor,
  restoreAfterFailure,
  review,
  submission,
  withCustomBaseUrl,
  withMode,
  withProviderName,
  withUpstream,
  type Editor,
} from './sub2ApiDraft';
import type { Sub2ApiConfigState } from './types';

const notConfigured: Sub2ApiConfigState = {
  configured: false,
  mode: 'subscription',
  baseUrl: '',
  codexProvider: '',
  customBaseUrl: false,
  email: '',
  upstream: 'codex',
};

const namedSubscription: Sub2ApiConfigState = {
  configured: true,
  mode: 'subscription',
  upstream: 'codex',
  codexProvider: 'relay',
  baseUrl: 'https://relay.example.com',
  customBaseUrl: false,
  email: '',
  useProviderKey: false,
  reusableCredential: {
    baseUrl: 'https://relay.example.com',
    customBaseUrl: false,
    providerSource: 'codex',
    codexProvider: 'relay',
  },
};

const enteredSubscription: Sub2ApiConfigState = {
  configured: true,
  mode: 'subscription',
  upstream: 'grok',
  codexProvider: '',
  baseUrl: 'https://user.example.com',
  customBaseUrl: true,
  email: '',
  reusableCredential: {
    baseUrl: 'https://user.example.com',
    customBaseUrl: true,
    codexProvider: '',
  },
};

const admin: Sub2ApiConfigState = {
  configured: true,
  mode: 'admin',
  upstream: 'antigravity',
  providerSource: 'codex',
  codexProvider: 'relay',
  baseUrl: 'https://resolved.example.com',
  customBaseUrl: false,
  email: 'admin@example.com',
  reusableCredential: { baseUrl: 'https://resolved.example.com' },
};

const idle = { lookup: { resolving: false, failed: false }, saving: false };

function edit(editor: Editor, draft: Partial<Editor['draft']>): Editor {
  return { ...editor, draft: { ...editor.draft, ...draft } };
}

/** Switches to the saved administrator Track, with its provider looked up again. */
function adminTrack(state: Sub2ApiConfigState): Editor {
  const change = withMode(editorFor(state), 'admin');
  expect(change.lookup).toEqual({ kind: 'resolve', name: 'relay' });
  return edit(change.editor, { baseUrl: admin.baseUrl });
}

describe('sub2ApiDraft', () => {
  it('opens on the saved Track with a draft for each saved Track', () => {
    const editor = editorFor({ ...enteredSubscription, otherMode: admin });
    expect(editor.mode).toBe('subscription');
    expect(editor.upstream).toBe('grok');
    expect(editor.draft).toMatchObject({
      baseUrl: 'https://user.example.com',
      customBaseUrl: true,
      providerSource: 'grok',
    });
    expect(editor.drafts.admin).toMatchObject({ providerName: 'relay', email: admin.email });
  });

  it('leaves the other Track’s address empty on first use and keeps each Track’s edits', () => {
    const editor = editorFor(admin);
    const subscription = withMode(editor, 'subscription');
    expect(subscription.editor.upstream).toBe('antigravity');
    expect(subscription.editor.draft.baseUrl).toBe('');
    expect(subscription.lookup).toEqual({ kind: 'stop' });
    expect(review(subscription.editor, admin, idle).canSave).toBe(false);

    const typed = edit(subscription.editor, { baseUrl: 'https://typed.example.com' });
    const back = withMode(typed, 'admin');
    expect(back.editor.draft.providerName).toBe('relay');
    // A named provider is looked up again when its Track is shown.
    expect(back.lookup).toEqual({ kind: 'resolve', name: 'relay' });
    expect(withMode(back.editor, 'subscription').editor.draft.baseUrl).toBe(
      'https://typed.example.com',
    );
  });

  it('keeps an administrator’s named provider when the upstream changes', () => {
    const change = withUpstream(editorFor(admin), 'claude');
    expect(change.lookup).toEqual({ kind: 'keep' });
    expect(change.editor.upstream).toBe('claude');
    expect(change.editor.draft).toMatchObject({ providerName: 'relay', providerSource: 'codex' });
    expect(submission(change.editor, admin).input).toMatchObject({
      upstream: 'claude',
      providerSource: 'codex',
      codexProvider: 'relay',
      password: '',
    });
  });

  it('keeps edits to an entered administrator address across upstream changes', () => {
    let editor = withMode(editorFor(notConfigured), 'admin').editor;
    editor = withCustomBaseUrl(editor, true).editor;
    editor = edit(editor, {
      baseUrl: 'https://relay.example.com',
      email: 'admin@example.com',
      password: 'first',
    });
    editor = withUpstream(editor, 'claude').editor;
    expect(editor.draft.password).toBe('first');
    editor = withUpstream(edit(editor, { password: 'edited' }), 'codex').editor;
    expect(editor.draft).toMatchObject({
      customBaseUrl: true,
      baseUrl: 'https://relay.example.com',
      email: 'admin@example.com',
      password: 'edited',
    });
    expect(review(editor, notConfigured, idle).canSave).toBe(true);
  });

  it('enters the address for an upstream that names no providers', () => {
    const named = editorFor(namedSubscription);
    const change = withUpstream(named, 'grok');
    expect(change.lookup).toEqual({ kind: 'stop' });
    expect(change.editor.draft).toMatchObject({
      customBaseUrl: true,
      providerName: '',
      providerSource: 'grok',
      // Upstreams of one instance share the address already shown.
      baseUrl: 'https://relay.example.com',
    });
    expect(withUpstream(named, 'claude').lookup).toEqual({ kind: 'resolve', name: 'relay' });
  });

  it('looks up a typed provider name and starts the address over', () => {
    const editor = editorFor(namedSubscription);
    const typed = withProviderName(editor, ' other ');
    expect(typed.lookup).toEqual({ kind: 'resolve', name: 'other' });
    expect(typed.editor.draft.baseUrl).toBe('');
    expect(withProviderName(editor, '  ').lookup).toEqual({ kind: 'stop' });

    const custom = withCustomBaseUrl(editor, true);
    expect(custom.lookup).toEqual({ kind: 'stop' });
    expect(custom.editor.draft).toMatchObject({
      customBaseUrl: true,
      baseUrl: '',
      providerName: '',
    });
  });

  it('keeps a saved secret only where the backend says a save keeps it', () => {
    const named = editorFor(namedSubscription);
    expect(review(named, namedSubscription, idle)).toMatchObject({
      canReuseCredential: true,
      keepSavedKey: true,
      automaticKey: false,
    });
    // A trailing slash is the same address.
    const slash = edit(named, { baseUrl: 'https://relay.example.com/' });
    expect(review(slash, namedSubscription, idle).keepSavedKey).toBe(true);
    for (const changed of [
      edit(named, { baseUrl: 'https://other.example.com' }),
      edit(named, { providerName: 'other' }),
      edit(named, { providerSource: 'claude' }),
    ]) {
      expect(review(changed, namedSubscription, idle).canReuseCredential).toBe(false);
    }
    // Without a scope the backend keeps nothing, so the editor promises nothing.
    const noSecret = { ...namedSubscription, reusableCredential: undefined };
    expect(review(named, noSecret, idle).canReuseCredential).toBe(false);

    // An entered address keeps its key whichever upstream it is marked as.
    const entered = withUpstream(editorFor(enteredSubscription), 'antigravity').editor;
    expect(review(entered, enteredSubscription, idle).keepSavedKey).toBe(true);

    // Each Track reuses only its own saved secret.
    const otherTrack = withMode(editorFor(enteredSubscription), 'admin').editor;
    const withAdmin = { ...enteredSubscription, otherMode: admin };
    expect(review(otherTrack, enteredSubscription, idle).canReuseCredential).toBe(false);
    expect(review(adminTrack(withAdmin), withAdmin, idle).canReuseCredential).toBe(true);
  });

  it('clears a saved key only when asked, not when a typed key is deleted', () => {
    const named = editorFor(namedSubscription);
    const retyped = edit(edit(named, { apiKey: 'x' }), { apiKey: '' });
    expect(submission(retyped, namedSubscription).input.useProviderKey).toBeUndefined();
    const cleared = edit(named, { clearSavedKey: true });
    expect(review(cleared, namedSubscription, idle)).toMatchObject({
      keepSavedKey: false,
      automaticKey: true,
    });
    expect(submission(cleared, namedSubscription).input.useProviderKey).toBe(true);
  });

  it('saves only with an address, a login and a secret it can use', () => {
    const named = editorFor(namedSubscription);
    expect(review(named, namedSubscription, idle).canSave).toBe(true);
    expect(review(named, namedSubscription, { ...idle, saving: true }).canSave).toBe(false);
    for (const lookup of [
      { resolving: true, failed: false },
      { resolving: false, failed: true },
    ]) {
      expect(review(named, namedSubscription, { ...idle, lookup }).canSave).toBe(false);
    }
    // An entered address needs a key it can send.
    const elsewhere = edit(editorFor(enteredSubscription), {
      baseUrl: 'https://other.example.com',
    });
    expect(review(elsewhere, enteredSubscription, idle).canSave).toBe(false);
    expect(review(edit(elsewhere, { apiKey: 'key' }), enteredSubscription, idle).canSave).toBe(
      true,
    );

    const adminEditor = editorFor(admin);
    const resolved = { ...adminEditor };
    expect(review(resolved, admin, idle).canSave).toBe(true);
    expect(review(edit(resolved, { email: ' ' }), admin, idle).canSave).toBe(false);
    const moved = edit(resolved, { baseUrl: 'https://moved.example.com' });
    expect(review(moved, admin, idle).canSave).toBe(false);
    expect(review(edit(moved, { password: 'secret' }), admin, idle).canSave).toBe(true);
  });

  it('submits the draft and shows it until the backend answers', () => {
    const editor = edit(adminTrack({ ...enteredSubscription, otherMode: admin }), {
      password: 'secret',
      email: ' admin@example.com ',
    });
    const { input, optimistic } = submission(editor, { ...enteredSubscription, otherMode: admin });
    expect(input).toEqual({
      baseUrl: 'https://resolved.example.com',
      codexProvider: 'relay',
      providerSource: 'codex',
      customBaseUrl: false,
      mode: 'admin',
      email: 'admin@example.com',
      password: 'secret',
      upstream: 'grok',
    });
    expect(optimistic).toMatchObject({ configured: true, mode: 'admin', providerSource: 'codex' });
    // The backend's answer says which secret is kept; the saved Track's stays with it.
    expect(optimistic.reusableCredential).toBeUndefined();
    expect(optimistic.otherMode).toMatchObject({
      mode: 'subscription',
      reusableCredential: enteredSubscription.reusableCredential,
    });
    expect(optimistic.otherMode?.otherMode).toBeUndefined();
  });

  it('shows the submitted fields again after a failed save, without the secrets', () => {
    const editor = edit(editorFor(namedSubscription), { clearSavedKey: true, apiKey: '' });
    const { input } = submission(editor, namedSubscription);
    const restored = restoreAfterFailure(namedSubscription, input);
    expect(restored.mode).toBe('subscription');
    expect(restored.draft).toMatchObject({
      baseUrl: 'https://relay.example.com',
      providerName: 'relay',
      clearSavedKey: true,
      password: '',
      apiKey: '',
      revealPassword: false,
    });
  });
});
