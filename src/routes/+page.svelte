<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { message as dialogMessage, open } from "@tauri-apps/plugin-dialog";
  import { disable, enable, isEnabled } from "@tauri-apps/plugin-autostart";
  import { onMount } from "svelte";

  type Settings = {
    serverUrl: string;
    watchPaths: string[];
    syncEnabled: boolean;
    minimizeToTray: boolean;
    hasApiKey: boolean;
  };

  type SyncStatus = {
    running: boolean;
    lastError: string | null;
    lastUploadMs: number | null;
    localFilesTotal: number;
    localFilesUploaded: number;
    currentFile: string | null;
  };

  type StorageInfo = {
    serverDiskAvailableRaw: number | null;
    serverDiskUsagePercentage: number | null;
    serverDiskAvailableHuman: string | null;
    serverStorageForbidden: boolean;
    serverError: string | null;
  };

  let settings = $state<Settings | null>(null);
  let apiKeyInput = $state("");
  /** When false and a key exists, show a mask instead of an empty password box. */
  let editingApiKey = $state(false);
  let message = $state<string | null>(null);
  let messageKind = $state<"ok" | "err" | null>(null);
  let autostartOn = $state(false);
  let status = $state<SyncStatus | null>(null);
  let storageInfo = $state<StorageInfo | null>(null);
  let pollTimer: ReturnType<typeof setInterval> | null = null;
  let storagePollTimer: ReturnType<typeof setInterval> | null = null;
  /** Dedupe dialog alerts for the same lastError string. */
  let lastOfflineDialogError = $state<string | null>(null);

  function show(msg: string, kind: "ok" | "err") {
    message = msg;
    messageKind = kind;
  }

  /** Tauri invoke rejects with a value that often does not stringify to a useful message. */
  function invokeErr(e: unknown): string {
    if (typeof e === "string") return e;
    if (e instanceof Error) return e.message;
    if (e && typeof e === "object" && "message" in e) {
      const m = (e as { message: unknown }).message;
      if (typeof m === "string") return m;
    }
    return String(e);
  }

  async function refreshStorage() {
    try {
      storageInfo = await invoke<StorageInfo>("get_storage_info");
    } catch {
      storageInfo = null;
    }
  }

  async function refresh() {
    settings = await invoke<Settings>("get_settings");
    autostartOn = await isEnabled();
    status = await invoke<SyncStatus>("get_sync_status");
    // Do not await: storage fetch hits the network and must not block Save or typing when Immich is down.
    void refreshStorage();
  }

  $effect(() => {
    const e = status?.lastError;
    if (!e) {
      lastOfflineDialogError = null;
      return;
    }
    if (
      e.includes("Immich server is offline") &&
      e.includes("Sync stopped") &&
      e !== lastOfflineDialogError
    ) {
      lastOfflineDialogError = e;
      void dialogMessage(e, { title: "Immich server unreachable", kind: "error" });
    }
  });

  onMount(() => {
    void refresh();
    pollTimer = setInterval(() => {
      void invoke<SyncStatus>("get_sync_status").then((s) => (status = s));
    }, 2000);
    storagePollTimer = setInterval(() => {
      void refreshStorage();
    }, 15000);
    return () => {
      if (pollTimer) clearInterval(pollTimer);
      if (storagePollTimer) clearInterval(storagePollTimer);
    };
  });

  async function save() {
    if (!settings) return;
    try {
      await invoke("save_settings", {
        input: {
          serverUrl: settings.serverUrl,
          watchPaths: settings.watchPaths,
          syncEnabled: settings.syncEnabled,
          minimizeToTray: settings.minimizeToTray,
        },
      });
      if (apiKeyInput.trim()) {
        await invoke("set_api_key_cmd", { key: apiKeyInput.trim() });
        apiKeyInput = "";
        editingApiKey = false;
      }
      const wasAutostart = await isEnabled();
      if (autostartOn && !wasAutostart) {
        await enable();
      } else if (!autostartOn && wasAutostart) {
        await disable();
      }
      await refresh();
      show("Settings saved.", "ok");
    } catch (e) {
      show(invokeErr(e), "err");
    }
  }

  async function testConnection() {
    if (!settings) return;
    try {
      const v = await invoke<string>("test_connection", {
        input: {
          serverUrl: settings.serverUrl,
          apiKey: apiKeyInput.trim() || null,
        },
      });
      show(`Connected. Server version: ${v}`, "ok");
    } catch (e) {
      show(invokeErr(e), "err");
    }
  }

  async function pickFolders() {
    const selected = await open({ directory: true, multiple: true });
    if (!settings || selected === null) return;
    const paths = Array.isArray(selected) ? selected : [selected];
    const next = new Set([...settings.watchPaths, ...paths]);
    settings = { ...settings, watchPaths: [...next] };
  }

  function removePath(p: string) {
    if (!settings) return;
    settings = {
      ...settings,
      watchPaths: settings.watchPaths.filter((x) => x !== p),
    };
  }

  async function startSync() {
    if (!settings) return;
    try {
      const key = apiKeyInput.trim();
      if (key) {
        await invoke("set_api_key_cmd", { key });
        apiKeyInput = "";
        editingApiKey = false;
        await refresh();
      }
      await invoke("start_sync");
      await refresh();
      show("Sync started.", "ok");
    } catch (e) {
      show(invokeErr(e), "err");
    }
  }

  async function stopSync() {
    await invoke("stop_sync");
    await refresh();
    show("Sync stopped.", "ok");
  }

  async function clearKey() {
    try {
      await invoke("clear_api_key_cmd");
      apiKeyInput = "";
      editingApiKey = false;
      await refresh();
      show("API key cleared.", "ok");
    } catch (e) {
      show(invokeErr(e), "err");
    }
  }

  function formatTime(ms: number | null) {
    if (ms === null) return "—";
    return new Date(ms).toLocaleString();
  }

  function formatBytes(n: number | null | undefined) {
    if (n == null || !Number.isFinite(n)) return "—";
    const units = ["B", "KB", "MB", "GB", "TB"];
    let v = n;
    let i = 0;
    while (v >= 1024 && i < units.length - 1) {
      v /= 1024;
      i++;
    }
    return `${v.toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
  }
</script>

<main class="wrap">
  <header class="head">
    <h1>Immich Sync</h1>
    <p class="sub">
      Watch local folders and upload new photos and videos to your Immich server in the background.
    </p>
  </header>

  {#if settings}
    <section class="card">
      <h2>Server</h2>
      <label class="field">
        <span>Immich URL</span>
        <input
          type="url"
          placeholder="http://127.0.0.1:2283"
          bind:value={settings.serverUrl}
        />
      </label>
      <label class="field">
        <span>API key</span>
        {#if settings.hasApiKey && !editingApiKey}
          <div class="key-stored">
            <input
              type="text"
              readonly
              class="readonly-mask"
              value="••••••••••••••••••••"
              aria-label="API key saved"
            />
            <button
              type="button"
              class="secondary"
              onclick={() => {
                editingApiKey = true;
                apiKeyInput = "";
              }}>Change…</button>
          </div>
          <span class="field-hint"
            >A key is saved (Credential Manager and app data). Use Change to replace it.</span
          >
        {:else}
          <input
            type="password"
            autocomplete="off"
            placeholder={settings.hasApiKey ? "Paste new API key" : "Paste API key"}
            bind:value={apiKeyInput}
          />
          {#if settings.hasApiKey && editingApiKey}
            <button
              type="button"
              class="ghost"
              onclick={() => {
                editingApiKey = false;
                apiKeyInput = "";
              }}>Cancel</button
            >
          {/if}
          <span class="field-hint"
            >Stored in Windows Credential Manager and a local backup file next to your settings.</span
          >
        {/if}
      </label>
      <div class="row">
        <button type="button" class="secondary" onclick={testConnection}>Test connection</button>
        {#if settings.hasApiKey}
          <button type="button" class="ghost" onclick={clearKey}>Clear API key</button>
        {/if}
      </div>
    </section>

    <section class="card">
      <h2>Folders</h2>
      <p class="hint">Recursive watch — new or changed media files are uploaded.</p>
      <ul class="paths">
        {#each settings.watchPaths as p (p)}
          <li>
            <code>{p}</code>
            <button type="button" class="ghost" onclick={() => removePath(p)}>Remove</button>
          </li>
        {/each}
      </ul>
      <button type="button" class="secondary" onclick={pickFolders}>Add folders…</button>
    </section>

    <section class="card">
      <h2>Background</h2>
      <label class="check">
        <input type="checkbox" bind:checked={settings.syncEnabled} />
        <span>Enable sync when the app runs</span>
      </label>
      <label class="check">
        <input type="checkbox" bind:checked={settings.minimizeToTray} />
        <span>Minimize and close hide to system tray</span>
      </label>
      <p class="field-hint tray-hint">
        When enabled, the window hides instead of closing; use the tray icon to open or quit.
      </p>
      <label class="check">
        <input type="checkbox" bind:checked={autostartOn} />
        <span>Start Immich Sync when Windows logs in</span>
      </label>
    </section>

    <section class="card">
      <h2>Sync engine</h2>
      {#if storageInfo}
        <dl class="status storage-info">
          <dt>Immich server disk</dt>
          <dd>
            {#if storageInfo.serverError}
              <span class="err-inline">Could not load: {storageInfo.serverError}</span>
            {:else if !settings.hasApiKey}
              —
              <span class="path-hint">Save an API key to query server storage.</span>
            {:else if storageInfo.serverStorageForbidden}
              Not shown — key needs the <code>server.storage</code> permission (Immich API key scopes).
            {:else if storageInfo.serverDiskUsagePercentage != null}
              {storageInfo.serverDiskAvailableHuman ?? formatBytes(storageInfo.serverDiskAvailableRaw)} free
              ({storageInfo.serverDiskUsagePercentage.toFixed(1)}% used)
            {:else}
              —
            {/if}
          </dd>
        </dl>
      {/if}
      {#if status}
        <dl class="status">
          <dt>Running</dt>
          <dd>{status.running ? "Yes" : "No"}</dd>
          <dt>Local library</dt>
          <dd>
            {status.localFilesUploaded} of {status.localFilesTotal} supported files uploaded
          </dd>
          <dt>Working on</dt>
          <dd class="current-file" title={status.currentFile ?? ""}>
            {status.currentFile ?? "—"}
          </dd>
          <dt>Last upload</dt>
          <dd>{formatTime(status.lastUploadMs)}</dd>
          <dt>Last error</dt>
          <dd class:error={!!status.lastError}>{status.lastError ?? "—"}</dd>
        </dl>
      {/if}
      <div class="row">
        <button type="button" onclick={startSync} disabled={status?.running}>Start sync</button>
        <button type="button" class="secondary" onclick={stopSync} disabled={!status?.running}>
          Stop sync
        </button>
      </div>
    </section>

    <div class="footer">
      <button type="button" class="primary" onclick={save}>Save settings</button>
      {#if message}
        <p class="msg" class:ok={messageKind === "ok"} class:err={messageKind === "err"}>{message}</p>
      {/if}
    </div>
  {:else}
    <p class="loading">Loading…</p>
  {/if}
</main>

<style>
  :global(body) {
    margin: 0;
    font-family:
      system-ui,
      "Segoe UI",
      Roboto,
      sans-serif;
    background: #1a1b1e;
    color: #e9e9ef;
    min-height: 100vh;
  }

  .wrap {
    max-width: 640px;
    margin: 0 auto;
    padding: 28px 20px 48px;
  }

  .head h1 {
    margin: 0 0 8px;
    font-size: 1.5rem;
    font-weight: 600;
    letter-spacing: -0.02em;
  }

  .sub {
    margin: 0 0 24px;
    color: #a1a1b0;
    font-size: 0.95rem;
    line-height: 1.45;
  }

  .card {
    background: #25262b;
    border: 1px solid #373a40;
    border-radius: 10px;
    padding: 18px 18px 16px;
    margin-bottom: 16px;
  }

  .card h2 {
    margin: 0 0 14px;
    font-size: 0.85rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: #909296;
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: 6px;
    margin-bottom: 14px;
  }

  .field span {
    font-size: 0.88rem;
    color: #c1c2c5;
  }

  .field-hint {
    font-size: 0.78rem;
    color: #868e96;
    line-height: 1.35;
    margin-top: -4px;
  }

  .tray-hint {
    margin: -4px 0 10px 28px;
  }

  .key-stored {
    display: flex;
    gap: 10px;
    align-items: center;
    flex-wrap: wrap;
  }

  .readonly-mask {
    flex: 1;
    min-width: 12rem;
    cursor: default;
    letter-spacing: 0.12em;
    color: #adb5bd;
  }

  input[type="url"],
  input[type="password"] {
    padding: 10px 12px;
    border-radius: 8px;
    border: 1px solid #373a40;
    background: #1a1b1e;
    color: #f1f3f5;
    font-size: 0.95rem;
  }

  input:focus {
    outline: 2px solid #4c6ef5;
    outline-offset: 0;
    border-color: #4c6ef5;
  }

  .hint {
    margin: 0 0 12px;
    font-size: 0.88rem;
    color: #909296;
  }

  .paths {
    list-style: none;
    padding: 0;
    margin: 0 0 12px;
  }

  .paths li {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 10px;
    padding: 8px 0;
    border-bottom: 1px solid #373a40;
    font-size: 0.82rem;
  }

  .paths li:last-child {
    border-bottom: none;
  }

  code {
    word-break: break-all;
    color: #adb5bd;
  }

  .check {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-bottom: 10px;
    cursor: pointer;
    font-size: 0.92rem;
  }

  .check input {
    width: 18px;
    height: 18px;
    accent-color: #4c6ef5;
  }

  button {
    padding: 9px 16px;
    border-radius: 8px;
    border: 1px solid #373a40;
    background: #373a40;
    color: #f1f3f5;
    font-size: 0.9rem;
    cursor: pointer;
  }

  button:hover:not(:disabled) {
    background: #4a4d55;
    border-color: #4a4d55;
  }

  button:disabled {
    opacity: 0.45;
    cursor: not-allowed;
  }

  button.primary {
    background: #4c6ef5;
    border-color: #4c6ef5;
    font-weight: 500;
  }

  button.primary:hover:not(:disabled) {
    background: #5c7cfa;
    border-color: #5c7cfa;
  }

  button.secondary {
    background: transparent;
    border-color: #4a4d55;
  }

  button.ghost {
    padding: 6px 10px;
    font-size: 0.82rem;
    background: transparent;
    border: none;
    color: #a1a1b0;
  }

  button.ghost:hover:not(:disabled) {
    color: #f1f3f5;
    background: rgba(255, 255, 255, 0.06);
  }

  .row {
    display: flex;
    flex-wrap: wrap;
    gap: 10px;
    align-items: center;
  }

  .status {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 6px 16px;
    margin: 0 0 14px;
    font-size: 0.88rem;
  }

  .status dt {
    margin: 0;
    color: #909296;
  }

  .status dd {
    margin: 0;
    font-family: ui-monospace, monospace;
    font-size: 0.84rem;
  }

  .status dd.error {
    color: #fa5252;
  }

  .storage-info dd {
    font-family: inherit;
  }

  .path-hint {
    display: inline;
    margin-left: 6px;
    font-size: 0.82rem;
    color: #868e96;
  }

  .err-inline {
    color: #f783ac;
    font-size: 0.84rem;
  }

  .status dd.current-file {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 100%;
  }

  .footer {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 10px;
    margin-top: 8px;
  }

  .msg {
    margin: 0;
    font-size: 0.88rem;
  }

  .msg.ok {
    color: #51cf66;
  }

  .msg.err {
    color: #ff8787;
  }

  .loading {
    color: #909296;
  }
</style>
