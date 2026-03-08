import type { SourceFlavor } from "./wasmRuntime";

export const MARKER_OWNER = "pd-vm-playground-lint";
export const BREAKPOINT_GLYPH_CLASS = "pd-debug-breakpoint-glyph";
export const CURRENT_LINE_CLASS = "pd-debug-current-line";
export const CURRENT_LINE_MARKER_CLASS = "pd-debug-current-line-marker";
export const THEME_STORAGE_KEY = "pd-vm-webui-theme";
export const FLAVOR_STORAGE_KEY = "pd-vm-webui-flavor";
export const SOURCE_STORAGE_KEY_PREFIX = "pd-vm-webui-source:";
const VIEWPORT_HEIGHT_CSS_VAR = "--pd-app-height";

export const RUN_POLL_INTERVAL_MS = 25;
export const EPOCH_TICK_INTERVAL_MS = 1;
export const EPOCH_UI_REFRESH_INTERVAL_MS = 250;
export const EPOCH_FLUSH_INTERVAL_MS = 250;

export const DEFAULT_FUEL_HINT =
  "New runs and debug sessions start with interruption disabled unless you explicitly arm fuel or epoch.";

export type InterruptModeChoice = "none" | "fuel" | "epoch";
export type ThemePreference = "light" | "dark" | "system";
export type ResolvedTheme = "light" | "dark";

export const FLAVOR_OPTIONS: Array<{ value: SourceFlavor; label: string }> = [
  { value: "rustscript", label: "RustScript (.rss)" },
  { value: "javascript", label: "JavaScript (.js)" },
  { value: "lua", label: "Lua (.lua)" },
  { value: "scheme", label: "Scheme (.scm)" }
];

export const THEME_OPTIONS: Array<{
  value: ThemePreference;
  label: string;
  icon: string;
  title: string;
}> = [
  { value: "system", label: "System", icon: "theme_system", title: "Follow system theme" },
  { value: "light", label: "Light", icon: "theme_light", title: "Light mode" },
  { value: "dark", label: "Dark", icon: "theme_dark", title: "Dark mode" }
];

export const SAMPLE_SOURCES: Record<SourceFlavor, string> = {
  rustscript: `
use stdlib::rss::strings as string;

use re;
use json;
use runtime;

// Complex RustScript example with closure capture, stdlib module use, and host calls.
let mut total = 0;
for (let mut i = 0; i < 4; i = i + 1) {
    total = total + i;
}

runtime::sleep(100);

let total = if !string::non_empty("rustscript") => {
    let zeroed = 0;
    zeroed
} else => {
    let bumped = total + 1;
    bumped
};

let mut base = 7;
let add = |value| value + base;
base = 8;
let mut closure_value = add(5);

let profile = { stats: { score: closure_value } };
let chained_score = profile?.stats?.score;
let missing_score = profile?.missing?.value;

let matched = match chained_score {
    12 => closure_value,
    _ => 0,
};

let regex_ok = re::match("^rustscript$", "RUSTSCRIPT", "i");
let payload = {
    lang: "rustscript",
    score: closure_value,
    matched: matched,
};
let payload_json = json::encode(payload);
let payload_decoded = json::decode(payload_json);
let json_score = payload_decoded.score;

if regex_ok && json_score == matched {
    print("closure_value is {:3}", closure_value);
} else {
    print(0);
}
`,
  javascript: ["let value = 21;", "console.log(value + 21);", "value + 21;"].join("\n"),
  lua: ["local value = 21", "print(value + 21)", "value + 21"].join("\n"),
  scheme: ["(define value 21)", "(print (+ value 21))", "(+ value 21)"].join("\n")
};

export function languageForFlavor(flavor: SourceFlavor): string {
  if (flavor === "rustscript") {
    return "rustscript";
  }
  if (flavor === "javascript") {
    return "javascript";
  }
  if (flavor === "lua") {
    return "lua";
  }
  return "scheme";
}

function isThemePreference(value: string): value is ThemePreference {
  return value === "light" || value === "dark" || value === "system";
}

function isSourceFlavor(value: string): value is SourceFlavor {
  return FLAVOR_OPTIONS.some((option) => option.value === value);
}

export function sourceStorageKey(flavor: SourceFlavor): string {
  return `${SOURCE_STORAGE_KEY_PREFIX}${flavor}`;
}

export function loadThemePreference(): ThemePreference {
  try {
    const stored = window.localStorage.getItem(THEME_STORAGE_KEY);
    if (stored && isThemePreference(stored)) {
      return stored;
    }
  } catch {
    // Ignore storage failures and fall back to the system preference.
  }
  return "system";
}

export function loadCurrentFlavor(): SourceFlavor {
  try {
    const stored = window.localStorage.getItem(FLAVOR_STORAGE_KEY);
    if (stored && isSourceFlavor(stored)) {
      return stored;
    }
  } catch {
    // Ignore storage failures and fall back to the default flavor.
  }
  return "rustscript";
}

export function loadSourceForFlavor(flavor: SourceFlavor): string {
  try {
    const stored = window.localStorage.getItem(sourceStorageKey(flavor));
    if (stored !== null) {
      return stored;
    }
  } catch {
    // Ignore storage failures and fall back to the bundled sample.
  }
  return SAMPLE_SOURCES[flavor];
}

export function resolveTheme(
  preference: ThemePreference,
  query: MediaQueryList | null
): ResolvedTheme {
  if (preference === "system") {
    return query?.matches ? "dark" : "light";
  }
  return preference;
}

export function applyDocumentTheme(theme: ResolvedTheme): void {
  document.documentElement.dataset.theme = theme;
  document.documentElement.style.colorScheme = theme;
}

export function updateViewportHeightCssVar(): void {
  const viewport = window.visualViewport;
  const height = viewport?.height ?? window.innerHeight;
  document.documentElement.style.setProperty(
    VIEWPORT_HEIGHT_CSS_VAR,
    `${Math.round(height)}px`
  );
}
