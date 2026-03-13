import { describe, expect, test } from "bun:test";

import { ensureRustScriptLanguage } from "../src/monaco/rustscriptLanguage";

type MonarchRule = [RegExp | string, unknown, ...(string | string[])[]];

function createMonacoMock() {
  const languages: Array<{ id: string }> = [];
  let provider: { tokenizer: Record<string, MonarchRule[]> } | null = null;
  let config: unknown = null;

  return {
    languages: {
      getLanguages() {
        return languages;
      },
      register(language: { id: string }) {
        languages.push(language);
      },
      setMonarchTokensProvider(_languageId: string, nextProvider: { tokenizer: Record<string, MonarchRule[]> }) {
        provider = nextProvider;
      },
      setLanguageConfiguration(_languageId: string, nextConfig: unknown) {
        config = nextConfig;
      }
    },
    getProvider() {
      return provider;
    },
    getConfig() {
      return config;
    }
  };
}

function stateHasMatch(
  provider: { tokenizer: Record<string, MonarchRule[]> },
  state: string,
  text: string
): boolean {
  for (const rule of provider.tokenizer[state] ?? []) {
    const pattern = rule[0];
    if (pattern instanceof RegExp && pattern.test(text)) {
      return true;
    }
  }
  return false;
}

const monaco = createMonacoMock();
ensureRustScriptLanguage(monaco as never);
const provider = monaco.getProvider();

if (!provider) {
  throw new Error("RustScript language provider was not registered");
}

describe("RustScript generic highlighting", () => {
  test("registers language configuration", () => {
    expect(monaco.getConfig()).toBeTruthy();
  });

  test("matches generic struct and function headers", () => {
    expect(stateHasMatch(provider, "root", "struct Box<T> {")).toBe(true);
    expect(stateHasMatch(provider, "root", "struct Cache<K, V> {")).toBe(true);
    expect(stateHasMatch(provider, "root", "fn myfn<T>(v: T) {")).toBe(true);
    expect(stateHasMatch(provider, "root", "pub fn wrap<K, V>(value: V) {")).toBe(true);
  });

  test("matches generic annotations in signatures and fields", () => {
    expect(stateHasMatch(provider, "functionSignature", "value: T)")).toBe(true);
    expect(stateHasMatch(provider, "functionSignature", "-> Box<string> {")).toBe(true);
    expect(stateHasMatch(provider, "structBlock", "value: T,")).toBe(true);
    expect(stateHasMatch(provider, "structBlock", "nodes: map<LruNode<K, V>>,")).toBe(true);
    expect(stateHasMatch(provider, "root", "let boxed: Box<string> = value;")).toBe(true);
    expect(stateHasMatch(provider, "root", "let rows: [LruEntryRow<K, V>] = [];")).toBe(true);
  });

  test("matches turbofish calls", () => {
    expect(stateHasMatch(provider, "root", "myfn::<string>(value)")).toBe(true);
    expect(stateHasMatch(provider, "root", "json::decode::<Profile>(payload)")).toBe(true);
    expect(stateHasMatch(provider, "root", "lrucache::new::<string, int>(2)")).toBe(true);
  });
});
