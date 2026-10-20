import { useEffect, useMemo, useState } from "react";
import { AlertTriangle, CheckCircle2, CircleAlert, KeyRound, Loader2, X } from "lucide-react";
import { api, ImmichSettings, ImmichTestResult } from "../api";
import { EMPTY, insecureRemote, suggestPrefix } from "../immich-form";
import { useStore } from "../store";
import { Overlay, Section, Toggle } from "./ui";

export default function SettingsDialog() {
  const open = useStore((s) => s.settingsOpen);
  const setOpen = useStore((s) => s.setSettingsOpen);
  const loadImmich = useStore((s) => s.loadImmich);
  const appSettings = useStore((s) => s.settings);
  const selected = useStore((s) => s.selected);

  const [form, setForm] = useState<ImmichSettings>(EMPTY);
  const [hasKey, setHasKey] = useState(false);
  const [permission, setPermission] = useState("asset.read");
  const [apiKey, setApiKey] = useState("");
  const [replacingKey, setReplacingKey] = useState(false);
  const [test, setTest] = useState<ImmichTestResult | null>(null);
  const [testing, setTesting] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [exampleRel, setExampleRel] = useState("");

  useEffect(() => {
    if (!open) return;
    setTest(null);
    setError(null);
    setApiKey("");
    setReplacingKey(false);
    api.getImmichSettings().then(({ hasKey, requiredPermission, ...s }) => {
      setForm(s);
      setHasKey(hasKey);
      setPermission(requiredPermission);
    });
  }, [open]);

  const firstSelected = useMemo(() => (selected.size ? Math.min(...selected) : null), [selected]);
  useEffect(() => {
    if (!open) return;
    api
      .examplePath(appSettings.folderTpl, appSettings.nameTpl, firstSelected)
      .then((e) => setExampleRel(e.path.replace(/\\/g, "/")));
  }, [open, appSettings.folderTpl, appSettings.nameTpl, firstSelected]);

  if (!open) return null;

  const update = (patch: Partial<ImmichSettings>) => setForm((f) => ({ ...f, ...patch }));
  const keyInput = !hasKey || replacingKey;
  const keyToSend = keyInput && apiKey.trim() ? apiKey.trim() : null;
  const prefix = form.pathPrefix.trim().replace(/\/+$/, "");
  const expectedPath = prefix ? `${prefix}/${exampleRel}` : `…/${exampleRel}`;
  const suggestion = test?.samplePath && exampleRel ? suggestPrefix(test.samplePath, exampleRel) : null;

  const runTest = async () => {
    setTesting(true);
    setTest(null);
    try {
      setTest(await api.testImmich(form, keyToSend));
    } finally {
      setTesting(false);
    }
  };

  const save = async () => {
    setSaving(true);
    setError(null);
    try {
      await api.saveImmichSettings(form, keyToSend);
      await loadImmich();
      setOpen(false);
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  };

  const forgetKey = async () => {
    await api.forgetImmichKey();
    setHasKey(false);
    setReplacingKey(false);
    await loadImmich();
  };

  return (
    <Overlay width={540}>
      <div className="mb-4 flex items-center justify-between">
        <h2 className="text-base font-semibold">Settings</h2>
        <button className="text-text-secondary hover:text-text-primary" onClick={() => setOpen(false)}>
          <X size={18} />
        </button>
      </div>

      <div className="space-y-5">
        <Section title="Immich">
          <Toggle
            label="Check my Immich library for duplicates"
            hint="Photos that are new locally are looked up in Immich. Matches are treated as duplicates."
            checked={form.enabled}
            onChange={(v) => update({ enabled: v })}
          />

          <div className={`space-y-3 pt-1 ${form.enabled ? "" : "pointer-events-none opacity-40"}`}>
            <Field label="Server URL">
              <input
                value={form.url}
                spellCheck={false}
                placeholder="https://photos.example.com"
                onChange={(e) => update({ url: e.target.value })}
                className="input"
              />
              {insecureRemote(form.url) && (
                <p className="flex items-center gap-1.5 text-[11px] text-warning">
                  <AlertTriangle size={11} /> Plain http: the API key is sent unencrypted over the internet.
                </p>
              )}
            </Field>

            <Field label="API key">
              {keyInput ? (
                <input
                  type="password"
                  value={apiKey}
                  placeholder={hasKey ? "New API key" : "Paste API key"}
                  onChange={(e) => setApiKey(e.target.value)}
                  className="input font-mono"
                />
              ) : (
                <div className="flex items-center gap-2 rounded-md border border-border-color bg-surface px-2 py-1.5">
                  <KeyRound size={13} className="text-text-secondary" />
                  <span className="flex-1 text-text-secondary">Saved in the system keychain</span>
                  <button className="text-[12px] hover:text-text-primary" onClick={() => setReplacingKey(true)}>
                    Replace
                  </button>
                  <button className="text-[12px] text-danger hover:opacity-80" onClick={forgetKey}>
                    Remove
                  </button>
                </div>
              )}
              <p className="text-[11px] leading-relaxed text-text-secondary">
                In Immich, open <b>Account Settings → API Keys</b> and create a key with only the{" "}
                <code className="rounded bg-bg-primary px-1 text-text-primary">{permission}</code> permission. The
                importer only searches your library; it never changes anything in Immich.
              </p>
            </Field>

            <Toggle
              label="Accept self-signed certificates"
              hint="Only for servers you trust on your own network."
              checked={form.acceptInvalidCerts}
              onChange={(v) => update({ acceptInvalidCerts: v })}
            />

            <div className="flex items-start gap-3">
              <button
                onClick={runTest}
                disabled={testing || !form.url}
                className="shrink-0 whitespace-nowrap rounded-md bg-surface px-3 py-1.5 hover:bg-card-active disabled:opacity-40"
              >
                {testing ? (
                  <span className="flex items-center gap-2">
                    <Loader2 size={13} className="animate-spin" /> Testing…
                  </span>
                ) : (
                  "Test connection"
                )}
              </button>
              {test && <TestSummary test={test} permission={permission} />}
            </div>

            <div className="space-y-2 border-t border-border-color pt-3">
              <Toggle
                label="Also match the folder path"
                hint="Only count a photo as found if Immich has it in the same year/month folder this importer would use."
                checked={form.matchPath}
                onChange={(v) => update({ matchPath: v })}
              />
              {form.matchPath && (
                <>
                  <Field label="Immich path prefix (optional)">
                    <input
                      value={form.pathPrefix}
                      spellCheck={false}
                      placeholder="/mnt/photos"
                      onChange={(e) => update({ pathPrefix: e.target.value })}
                      className="input font-mono"
                    />
                    {suggestion && suggestion !== prefix && (
                      <p className="text-[11px] text-text-secondary">
                        Immich stores paths like <code className="text-text-primary">{test?.samplePath}</code>.{" "}
                        <button className="underline hover:text-text-primary" onClick={() => update({ pathPrefix: suggestion })}>
                          Use {suggestion}
                        </button>
                      </p>
                    )}
                  </Field>
                  <div className="rounded-md bg-surface px-3 py-2">
                    <div className="section-title mb-1">Expected Immich path</div>
                    <div className="break-all font-mono text-[11px]">{expectedPath}</div>
                    {!prefix && (
                      <p className="mt-1 text-[11px] text-text-secondary">Without a prefix, only the end of the path is compared.</p>
                    )}
                  </div>
                </>
              )}
            </div>
          </div>
        </Section>
      </div>

      {error && <p className="mt-4 text-[12px] text-danger">{error}</p>}
      <div className="mt-5 flex justify-end gap-2">
        <button className="rounded-md bg-surface px-4 py-1.5 hover:bg-card-active" onClick={() => setOpen(false)}>
          Cancel
        </button>
        <button
          className="rounded-md bg-accent px-4 py-1.5 font-semibold text-button-text disabled:bg-card-active disabled:text-text-secondary"
          disabled={saving || (form.enabled && (!form.url || (!hasKey && !apiKey.trim())))}
          onClick={save}
        >
          {saving ? "Saving…" : "Save"}
        </button>
      </div>
    </Overlay>
  );
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="space-y-1.5">
      <span className="block text-[12px] text-text-secondary">{label}</span>
      {children}
    </div>
  );
}

function TestSummary({ test, permission }: { test: ImmichTestResult; permission: string }) {
  if (test.ok) {
    return (
      <span className="flex items-start gap-1.5 pt-1.5 text-[12px] leading-snug text-success">
        <CheckCircle2 size={14} className="mt-px shrink-0" /> Connected to Immich {test.version}
        {test.keyName ? ` · key "${test.keyName}"` : ""} · {permission} ✓
      </span>
    );
  }
  return (
    <span className="flex items-start gap-1.5 pt-1.5 text-[12px] leading-snug text-danger">
      <CircleAlert size={14} className="mt-px shrink-0" /> {test.error}
    </span>
  );
}
