export type SidebarPanelKey = "diagnostics" | "output" | "stack" | "debug" | "fuel";

export interface PanelController {
  setCollapsed(panelKey: SidebarPanelKey, collapsed: boolean): void;
  toggle(panelKey: SidebarPanelKey): void;
  expand(panelKeys: SidebarPanelKey[]): void;
}

export interface PlaygroundUi {
  flavorSelectEl: HTMLSelectElement;
  themeControlEl: HTMLElement;
  themeSystemButtonEl: HTMLButtonElement;
  themeLightButtonEl: HTMLButtonElement;
  themeDarkButtonEl: HTMLButtonElement;
  runButtonEl: HTMLButtonElement;
  debugStartButtonEl: HTMLButtonElement;
  debugWhereButtonEl: HTMLButtonElement;
  debugLocalsButtonEl: HTMLButtonElement;
  debugStackButtonEl: HTMLButtonElement;
  debugStepButtonEl: HTMLButtonElement;
  debugNextButtonEl: HTMLButtonElement;
  debugOutButtonEl: HTMLButtonElement;
  debugContinueButtonEl: HTMLButtonElement;
  stopButtonEl: HTMLButtonElement;
  lintStatusEl: HTMLSpanElement;
  sessionStatusEl: HTMLSpanElement;
  loadSampleButtonEl: HTMLButtonElement;
  diagnosticsPanelEl: HTMLElement;
  outputPanelEl: HTMLElement;
  stackPanelEl: HTMLElement;
  debugOutputPanelEl: HTMLElement;
  debugHoverPanelEl: HTMLElement;
  interruptModeSelectEl: HTMLSelectElement;
  fuelAmountLabelEl: HTMLElement;
  fuelIntervalLabelEl: HTMLElement;
  fuelAmountInputEl: HTMLInputElement;
  fuelIntervalInputEl: HTMLInputElement;
  debugFuelSetButtonEl: HTMLButtonElement;
  debugFuelAddButtonEl: HTMLButtonElement;
  debugFuelIntervalButtonEl: HTMLButtonElement;
  debugEpochTickButtonEl: HTMLButtonElement;
  runResumeButtonEl: HTMLButtonElement;
  fuelHintPanelEl: HTMLElement;
  runFuelStatePanelEl: HTMLElement;
  runEpochStatePanelEl: HTMLElement;
  debugFuelStatePanelEl: HTMLElement;
  debugEpochStatePanelEl: HTMLElement;
  editorHostEl: HTMLElement;
  panelController: PanelController;
}

function iconSvg(content: string): string {
  return `<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${content}</svg>`;
}

const ICONS: Record<string, string> = {
  run: iconSvg('<polygon points="8 5 19 12 8 19 8 5" fill="currentColor" stroke="none"></polygon>'),
  debug: iconSvg(
    '<path d="M9 9h6"></path><path d="M9 15h6"></path><path d="M10 5.5 8.5 3.5"></path><path d="M14 5.5 15.5 3.5"></path><rect x="7" y="7" width="10" height="12" rx="4"></rect><path d="M4 9h3"></path><path d="M17 9h3"></path><path d="M3 13h4"></path><path d="M17 13h4"></path>'
  ),
  theme_system: iconSvg(
    '<rect x="3" y="5" width="18" height="12" rx="2"></rect><path d="M8 21h8"></path><path d="M12 17v4"></path>'
  ),
  theme_light: iconSvg(
    '<circle cx="12" cy="12" r="4"></circle><path d="M12 2v2.5"></path><path d="M12 19.5V22"></path><path d="m4.93 4.93 1.77 1.77"></path><path d="m17.3 17.3 1.77 1.77"></path><path d="M2 12h2.5"></path><path d="M19.5 12H22"></path><path d="m4.93 19.07 1.77-1.77"></path><path d="m17.3 6.7 1.77-1.77"></path>'
  ),
  theme_dark: iconSvg('<path d="M20 14.5A8.5 8.5 0 1 1 9.5 4 6.8 6.8 0 0 0 20 14.5"></path>'),
  diagnostics: iconSvg(
    '<path d="M12 3 21 19H3L12 3"></path><path d="M12 9v4"></path><circle cx="12" cy="16" r="1"></circle>'
  ),
  output: iconSvg(
    '<rect x="3" y="5" width="18" height="14" rx="2"></rect><path d="m7 10 3 2-3 2"></path><path d="M13 14h4"></path>'
  ),
  fuel: iconSvg(
    '<path d="M12 3c2.7 3.3 5 5.7 5 9a5 5 0 1 1-10 0c0-3.3 2.3-5.7 5-9"></path><path d="M10 14c.5 1 1.3 1.8 2.5 2.2"></path>'
  ),
  where: iconSvg(
    '<circle cx="12" cy="12" r="9"></circle><line x1="12" y1="3" x2="12" y2="7"></line><line x1="12" y1="17" x2="12" y2="21"></line><line x1="3" y1="12" x2="7" y2="12"></line><line x1="17" y1="12" x2="21" y2="12"></line>'
  ),
  locals: iconSvg(
    '<line x1="8" y1="6" x2="21" y2="6"></line><line x1="8" y1="12" x2="21" y2="12"></line><line x1="8" y1="18" x2="21" y2="18"></line><circle cx="4" cy="6" r="1.2"></circle><circle cx="4" cy="12" r="1.2"></circle><circle cx="4" cy="18" r="1.2"></circle>'
  ),
  stack: iconSvg(
    '<polygon points="12 2 2 7 12 12 22 7 12 2"></polygon><polyline points="2 12 12 17 22 12"></polyline><polyline points="2 17 12 22 22 17"></polyline>'
  ),
  chevron_down: iconSvg('<polyline points="6 9 12 15 18 9"></polyline>'),
  step: iconSvg('<line x1="5" y1="12" x2="19" y2="12"></line><polyline points="12 5 19 12 12 19"></polyline>'),
  next: iconSvg('<polyline points="7 17 12 12 7 7"></polyline><polyline points="13 17 18 12 13 7"></polyline>'),
  out: iconSvg('<polyline points="9 14 4 9 9 4"></polyline><path d="M20 20v-7a4 4 0 0 0-4-4H4"></path>'),
  continue: iconSvg('<polygon points="7 4 20 12 7 20 7 4" fill="currentColor" stroke="none"></polygon>'),
  stop: iconSvg('<rect x="5" y="5" width="14" height="14" rx="2" fill="currentColor" stroke="none"></rect>'),
  reset_sample: iconSvg(
    '<path d="M3 12a9 9 0 0 1 15.5-6.36L21 8"></path><path d="M21 3v5h-5"></path><path d="M21 12a9 9 0 0 1-15.5 6.36L3 16"></path><path d="M8 16H3v5"></path>'
  )
};

function mountIconButton(button: HTMLButtonElement, icon: string, label: string): void {
  button.innerHTML = `${ICONS[icon] ?? ""}<span class="sr-only">${label}</span>`;
}

function panelTitle(icon: string, label: string): string {
  return `<span class="panel-title-icon" aria-hidden="true">${ICONS[icon] ?? ""}</span><span>${label}</span>`;
}

function renderPanel(panelKey: SidebarPanelKey, icon: string, label: string, body: string): string {
  const bodyId = `panel-body-${panelKey}`;
  const collapsed = panelKey === "stack" || panelKey === "debug" || panelKey === "fuel";
  return `
    <article class="panel panel--${panelKey}" data-panel-key="${panelKey}" data-collapsed="${collapsed ? "true" : "false"}">
      <h2>
        <button
          id="panel-toggle-${panelKey}"
          class="panel-toggle"
          type="button"
          aria-expanded="${collapsed ? "false" : "true"}"
          aria-controls="${bodyId}"
        >
          ${panelTitle(icon, label)}
          <span class="panel-toggle-chevron" aria-hidden="true">${ICONS.chevron_down}</span>
        </button>
      </h2>
      <div id="${bodyId}" class="panel-body">
        ${body}
      </div>
    </article>
  `;
}

function queryRequired<T extends Element>(selector: string): T {
  const node = document.querySelector<T>(selector);
  if (!node) {
    throw new Error(`playground UI node is missing: ${selector}`);
  }
  return node;
}

function createPanelController(sidebarPanels: HTMLElement[]): PanelController {
  const sidebarPanelEls = new Map<SidebarPanelKey, HTMLElement>();
  const sidebarPanelToggleEls = new Map<SidebarPanelKey, HTMLButtonElement>();

  for (const panelEl of sidebarPanels) {
    const key = panelEl.dataset.panelKey as SidebarPanelKey | undefined;
    if (!key) {
      continue;
    }
    const toggleEl = panelEl.querySelector<HTMLButtonElement>(".panel-toggle");
    if (!toggleEl) {
      continue;
    }
    sidebarPanelEls.set(key, panelEl);
    sidebarPanelToggleEls.set(key, toggleEl);
    toggleEl.addEventListener("click", () => {
      const current = sidebarPanelEls.get(key);
      if (!current) {
        return;
      }
      setCollapsed(key, current.dataset.collapsed !== "true");
    });
  }

  function setCollapsed(panelKey: SidebarPanelKey, collapsed: boolean): void {
    const panelEl = sidebarPanelEls.get(panelKey);
    const toggleEl = sidebarPanelToggleEls.get(panelKey);
    if (!panelEl || !toggleEl) {
      return;
    }
    panelEl.dataset.collapsed = collapsed ? "true" : "false";
    toggleEl.setAttribute("aria-expanded", collapsed ? "false" : "true");
  }

  return {
    setCollapsed,
    toggle(panelKey) {
      const panelEl = sidebarPanelEls.get(panelKey);
      if (!panelEl) {
        return;
      }
      setCollapsed(panelKey, panelEl.dataset.collapsed !== "true");
    },
    expand(panelKeys) {
      for (const panelKey of panelKeys) {
        setCollapsed(panelKey, false);
      }
    }
  };
}

export function mountPlaygroundUi(
  app: HTMLDivElement,
  defaultFuelHint: string
): PlaygroundUi {
  app.innerHTML = `
    <main class="page">
      <section class="hero">
        <h1>RustScript playground</h1>
        <p>
        <span>Run or debug directly in wasm runtime with Monaco breakpoints, stepping controls, and hover variable inspect.</span>
        <span><a href="./about.html" style="color: #58a6ff; text-decoration: underline;">Read more about the VM & RustScript here.</a></span>
      </section>
      <section class="workspace">
        <div class="toolbar">
          <div class="flavor-control flavor-control--hidden" aria-hidden="true">
            <label for="flavor-select">Flavor</label>
            <select id="flavor-select" aria-label="source flavor"></select>
          </div>
          <button id="run-button" class="toolbar-action" type="button" title="Run" aria-label="Run"></button>
          <button id="debug-start-button" class="toolbar-action" type="button" title="Debug" aria-label="Debug"></button>
          <div class="debug-toolbar" role="toolbar" aria-label="debug controls">
            <button id="debug-where-button" class="icon-button icon-button--outline" type="button" title="Where" aria-label="Where"></button>
            <button id="debug-locals-button" class="icon-button icon-button--outline" type="button" title="Locals" aria-label="Locals"></button>
            <button id="debug-stack-button" class="icon-button icon-button--outline" type="button" title="Stack" aria-label="Stack"></button>
            <span class="toolbar-sep" aria-hidden="true"></span>
            <button id="debug-step-button" class="icon-button" type="button" title="Step" aria-label="Step"></button>
            <button id="debug-next-button" class="icon-button" type="button" title="Next" aria-label="Next"></button>
            <button id="debug-out-button" class="icon-button" type="button" title="Out" aria-label="Out"></button>
            <button id="debug-continue-button" class="icon-button" type="button" title="Continue" aria-label="Continue"></button>
            <span class="toolbar-sep" aria-hidden="true"></span>
            <button id="stop-button" class="icon-button icon-button--stop" type="button" title="Stop" aria-label="Stop"></button>
          </div>
          <div class="toolbar-status-strip" aria-live="polite">
            <span id="lint-status" class="status neutral">lint: idle</span>
            <span id="session-status" class="status neutral">idle</span>
            <div id="run-fuel-state" class="fuel-state-line" hidden>Run fuel: idle</div>
            <div id="run-epoch-state" class="fuel-state-line" hidden>Run epoch: idle</div>
            <div id="debug-fuel-state" class="fuel-state-line" hidden>Debug fuel: idle</div>
            <div id="debug-epoch-state" class="fuel-state-line" hidden>Debug epoch: idle</div>
          </div>
          <div class="toolbar-right">
            <button
              id="load-sample-button"
              class="toolbar-action toolbar-utility-button toolbar-utility-button--icon"
              type="button"
              title="Reset to Sample"
              aria-label="Reset to Sample"
            ></button>
            <div id="theme-control" class="theme-control" role="group" aria-label="theme mode" data-theme="system">
              <button id="theme-system-button" class="theme-option" type="button" title="Follow system theme" aria-label="Follow system theme"></button>
              <button id="theme-light-button" class="theme-option" type="button" title="Light mode" aria-label="Light mode"></button>
              <button id="theme-dark-button" class="theme-option" type="button" title="Dark mode" aria-label="Dark mode"></button>
            </div>
          </div>
        </div>
        <div class="workspace-body">
          <div class="editor-shell">
            <div id="editor" class="editor"></div>
          </div>
          <aside class="panels" aria-label="runtime details">
            ${renderPanel("diagnostics", "diagnostics", "Diagnostics", `
              <pre id="diagnostics" class="panel-content">No lint diagnostics.</pre>
            `)}
            ${renderPanel("output", "output", "Print Output", `
              <pre id="run-output" class="panel-content">&lt;no print output&gt;</pre>
            `)}
            ${renderPanel("stack", "stack", "Final Stack", `
              <pre id="run-stack" class="panel-content">&lt;empty stack&gt;</pre>
            `)}
            ${renderPanel("debug", "debug", "Debugger", `
              <pre id="debug-output" class="panel-content">&lt;no debugger output&gt;</pre>
              <div id="debug-hover" class="debug-hover">hover inspect: (none)</div>
            `)}
            ${renderPanel("fuel", "fuel", "Interruption", `
              <div class="fuel-panel">
                <label class="fuel-field" for="interrupt-mode-select">
                  <span>Mode</span>
                  <select id="interrupt-mode-select" class="fuel-input" aria-label="interruption mode">
                    <option value="none">Disabled</option>
                    <option value="fuel">Fuel</option>
                    <option value="epoch">Epoch</option>
                  </select>
                </label>
                <label class="fuel-field" for="fuel-amount-input">
                  <span id="fuel-amount-label">Fuel Amount</span>
                  <input
                    id="fuel-amount-input"
                    class="fuel-input"
                    type="number"
                    min="0"
                    step="1"
                    inputmode="numeric"
                    placeholder="disabled"
                  />
                </label>
                <label class="fuel-field" for="fuel-interval-input">
                  <span id="fuel-interval-label">Check Interval</span>
                  <input
                    id="fuel-interval-input"
                    class="fuel-input"
                    type="number"
                    min="1"
                    step="1"
                    inputmode="numeric"
                  />
                </label>
                <div class="fuel-actions">
                  <button id="debug-fuel-set-button" class="panel-button" type="button">Set Debug Fuel</button>
                  <button id="debug-fuel-add-button" class="panel-button panel-button--secondary" type="button">Add Debug Fuel</button>
                  <button id="debug-fuel-interval-button" class="panel-button panel-button--secondary" type="button">Apply Debug Interval</button>
                  <button id="debug-epoch-tick-button" class="panel-button panel-button--secondary" type="button">Pause Tick</button>
                  <button id="run-resume-button" class="panel-button" type="button">Resume Run</button>
                </div>
                <div id="fuel-hint" class="fuel-hint">${defaultFuelHint}</div>
              </div>
            `)}
          </aside>
        </div>
      </section>
    </main>
  `;

  const panelController = createPanelController(
    Array.from(document.querySelectorAll<HTMLElement>(".panel[data-panel-key]"))
  );

  const ui: PlaygroundUi = {
    flavorSelectEl: queryRequired("#flavor-select"),
    themeControlEl: queryRequired("#theme-control"),
    themeSystemButtonEl: queryRequired("#theme-system-button"),
    themeLightButtonEl: queryRequired("#theme-light-button"),
    themeDarkButtonEl: queryRequired("#theme-dark-button"),
    runButtonEl: queryRequired("#run-button"),
    debugStartButtonEl: queryRequired("#debug-start-button"),
    debugWhereButtonEl: queryRequired("#debug-where-button"),
    debugLocalsButtonEl: queryRequired("#debug-locals-button"),
    debugStackButtonEl: queryRequired("#debug-stack-button"),
    debugStepButtonEl: queryRequired("#debug-step-button"),
    debugNextButtonEl: queryRequired("#debug-next-button"),
    debugOutButtonEl: queryRequired("#debug-out-button"),
    debugContinueButtonEl: queryRequired("#debug-continue-button"),
    stopButtonEl: queryRequired("#stop-button"),
    lintStatusEl: queryRequired("#lint-status"),
    sessionStatusEl: queryRequired("#session-status"),
    loadSampleButtonEl: queryRequired("#load-sample-button"),
    diagnosticsPanelEl: queryRequired("#diagnostics"),
    outputPanelEl: queryRequired("#run-output"),
    stackPanelEl: queryRequired("#run-stack"),
    debugOutputPanelEl: queryRequired("#debug-output"),
    debugHoverPanelEl: queryRequired("#debug-hover"),
    interruptModeSelectEl: queryRequired("#interrupt-mode-select"),
    fuelAmountLabelEl: queryRequired("#fuel-amount-label"),
    fuelIntervalLabelEl: queryRequired("#fuel-interval-label"),
    fuelAmountInputEl: queryRequired("#fuel-amount-input"),
    fuelIntervalInputEl: queryRequired("#fuel-interval-input"),
    debugFuelSetButtonEl: queryRequired("#debug-fuel-set-button"),
    debugFuelAddButtonEl: queryRequired("#debug-fuel-add-button"),
    debugFuelIntervalButtonEl: queryRequired("#debug-fuel-interval-button"),
    debugEpochTickButtonEl: queryRequired("#debug-epoch-tick-button"),
    runResumeButtonEl: queryRequired("#run-resume-button"),
    fuelHintPanelEl: queryRequired("#fuel-hint"),
    runFuelStatePanelEl: queryRequired("#run-fuel-state"),
    runEpochStatePanelEl: queryRequired("#run-epoch-state"),
    debugFuelStatePanelEl: queryRequired("#debug-fuel-state"),
    debugEpochStatePanelEl: queryRequired("#debug-epoch-state"),
    editorHostEl: queryRequired("#editor"),
    panelController
  };

  mountIconButton(ui.runButtonEl, "run", "Run");
  mountIconButton(ui.debugStartButtonEl, "debug", "Debug");
  mountIconButton(ui.themeSystemButtonEl, "theme_system", "Follow system theme");
  mountIconButton(ui.themeLightButtonEl, "theme_light", "Light mode");
  mountIconButton(ui.themeDarkButtonEl, "theme_dark", "Dark mode");
  mountIconButton(ui.debugWhereButtonEl, "where", "Where");
  mountIconButton(ui.debugLocalsButtonEl, "locals", "Locals");
  mountIconButton(ui.debugStackButtonEl, "stack", "Stack");
  mountIconButton(ui.debugStepButtonEl, "step", "Step");
  mountIconButton(ui.debugNextButtonEl, "next", "Next");
  mountIconButton(ui.debugOutButtonEl, "out", "Out");
  mountIconButton(ui.debugContinueButtonEl, "continue", "Continue");
  mountIconButton(ui.stopButtonEl, "stop", "Stop");
  mountIconButton(ui.loadSampleButtonEl, "reset_sample", "Reset to Sample");

  return ui;
}
