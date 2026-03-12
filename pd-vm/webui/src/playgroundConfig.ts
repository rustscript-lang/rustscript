import complexExampleSource from "./examples/rss-complex-example.rss?raw";
import ifftExampleSource from "./examples/rss-ifft-example.rss?raw";
import collectionsIterExampleSource from "./examples/rss-collections-iter-example.rss?raw";
import lruCacheExampleSource from "./examples/rss-lrucache-example.rss?raw";
import stringsRegexExampleSource from "./examples/rss-strings-regex-example.rss?raw";

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
export type RssProgramKey =
  | "complex"
  | "ifft"
  | "lrucache"
  | "collections_iter"
  | "strings_regex";

export interface RssProgramOption {
  key: RssProgramKey;
  label: string;
  description: string;
  source: string;
}

export const DEFAULT_RSS_PROGRAM_KEY: RssProgramKey = "complex";
export const CUSTOM_RSS_PROGRAM_KEY = "__custom__";

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

export const RSS_PROGRAM_OPTIONS: RssProgramOption[] = [
  {
    key: "complex",
    label: "Demo",
    description: "Default playground demo with closures, structs, option matching, JSON, regex, and runtime host calls.",
    source: complexExampleSource.trim()
  },
  {
    key: "ifft",
    label: "IFFT Example",
    description: "A meaningful inverse FFT implementation using arrays, loops, math helpers, and validation.",
    source: ifftExampleSource.trim()
  },
  {
    key: "lrucache",
    label: "LRU Cache Example",
    description: "Builds a rolling feed cache with stdlib lrucache operations, recency updates, and structured output.",
    source: lruCacheExampleSource.trim()
  },
  {
    key: "collections_iter",
    label: "Collections + Iter",
    description: "Transforms channel metrics with collections and iter helpers into a dashboard snapshot.",
    source: collectionsIterExampleSource.trim()
  },
  {
    key: "strings_regex",
    label: "Strings + Regex",
    description: "Parses log lines with string helpers, regex captures, and string rewriting into a summary report.",
    source: stringsRegexExampleSource.trim()
  }
];

const DEFAULT_RSS_PROGRAM_SOURCE =
  RSS_PROGRAM_OPTIONS.find((option) => option.key === DEFAULT_RSS_PROGRAM_KEY)?.source ??
  complexExampleSource.trim();

export const SAMPLE_SOURCES: Record<SourceFlavor, string> = {
  rustscript: DEFAULT_RSS_PROGRAM_SOURCE,
  javascript: ["let value = 21;", "console.log(value + 21);", "value + 21;"].join("\n"),
  lua: ["local value = 21", "print(value + 21)", "value + 21"].join("\n"),
  scheme: ["(define value 21)", "(print (+ value 21))", "(+ value 21)"].join("\n")
};

export function rssProgramByKey(key: string): RssProgramOption | null {
  return RSS_PROGRAM_OPTIONS.find((option) => option.key === key) ?? null;
}

export function matchRssProgramSource(source: string): RssProgramOption | null {
  return RSS_PROGRAM_OPTIONS.find((option) => option.source === source.trim()) ?? null;
}

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
