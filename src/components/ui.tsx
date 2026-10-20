// Small building blocks shared by panels and dialogs.

export function Overlay({ children, width = 460 }: { children: React.ReactNode; width?: number }) {
  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-[2px]">
      <div
        className="max-h-[90vh] overflow-y-auto rounded-lg border border-border-color bg-bg-secondary p-5 shadow-2xl"
        style={{ width }}
      >
        {children}
      </div>
    </div>
  );
}

export function Section({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <section className="space-y-2">
      <h3 className="section-title">{title}</h3>
      {children}
    </section>
  );
}

export function Toggle({ label, hint, checked, onChange }: { label: string; hint: string; checked: boolean; onChange: (v: boolean) => void }) {
  return (
    <label className="flex cursor-pointer items-start gap-3">
      <button
        role="switch"
        aria-checked={checked}
        onClick={() => onChange(!checked)}
        className={`relative mt-0.5 h-5 w-9 shrink-0 rounded-full transition-colors ${checked ? "bg-accent" : "bg-card-active"}`}
      >
        <span
          className={`absolute top-0.5 h-4 w-4 rounded-full transition-all ${
            checked ? "left-[18px] bg-button-text" : "left-0.5 bg-text-secondary"
          }`}
        />
      </button>
      <span>
        <span className="block">{label}</span>
        <span className="mt-0.5 block text-[11px] leading-snug text-text-secondary">{hint}</span>
      </span>
    </label>
  );
}
