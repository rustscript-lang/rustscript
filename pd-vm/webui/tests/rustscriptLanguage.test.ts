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

function stateHasFullCommentMatch(
  provider: { tokenizer: Record<string, MonarchRule[]> },
  state: string,
  text: string
): boolean {
  for (const rule of provider.tokenizer[state] ?? []) {
    const pattern = rule[0];
    if (!(pattern instanceof RegExp) || rule[1] !== "comment") {
      continue;
    }
    const match = pattern.exec(text);
    if (match?.[0] === text) {
      return true;
    }
  }
  return false;
}

function anchoredMatch(pattern: RegExp, text: string): RegExpExecArray | null {
  const flags = pattern.flags.replaceAll("g", "").replaceAll("y", "");
  return new RegExp(`^(?:${pattern.source})`, flags).exec(text);
}

function tokenizeLine(
  provider: { tokenizer: Record<string, MonarchRule[]> },
  text: string,
  initialState = "root"
): Array<{ token: string; value: string }> {
  const stateStack = [initialState];
  const tokens: Array<{ token: string; value: string }> = [];
  let offset = 0;
  let guard = 0;

  while (offset < text.length && guard < text.length * 10) {
    guard += 1;
    const state = stateStack[stateStack.length - 1];
    const rules = provider.tokenizer[state] ?? [];
    let matched = false;

    for (const rule of rules) {
      const pattern = rule[0];
      if (!(pattern instanceof RegExp)) {
        continue;
      }
      const match = anchoredMatch(pattern, text.slice(offset));
      if (!match) {
        continue;
      }

      const action = rule[1];
      if (typeof action === "string") {
        if (action.length > 0 && match[0].length > 0) {
          tokens.push({ token: action, value: match[0] });
        }
      } else if (Array.isArray(action)) {
        for (let index = 0; index < action.length; index += 1) {
          const token = action[index];
          const value = match[index + 1] ?? "";
          if (typeof token === "string" && token.length > 0 && value.length > 0) {
            tokens.push({ token, value });
          }
        }
      }

      const next = rule[2];
      if (typeof next === "string") {
        if (next === "@pop") {
          if (stateStack.length > 1) {
            stateStack.pop();
          }
        } else if (next === "@push") {
          stateStack.push(state);
        } else if (next.startsWith("@")) {
          stateStack[stateStack.length - 1] = next.slice(1);
        }
      }

      offset += match[0].length;
      if (match[0].length === 0) {
        offset += 1;
      }
      matched = true;
      break;
    }

    if (!matched) {
      offset += 1;
    }
  }

  return tokens;
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

  test("matches full line comments instead of only the opener", () => {
    expect(stateHasFullCommentMatch(provider, "root", "// comment body")).toBe(true);
    expect(stateHasFullCommentMatch(provider, "functionSignature", "// comment body")).toBe(true);
    expect(stateHasFullCommentMatch(provider, "structBlock", "// comment body")).toBe(true);
  });

  test("tokenizes generic punctuation separately from type identifiers", () => {
    const tokens = tokenizeLine(provider, "let mut detached: LruNode<K, V> = node;");
    expect(tokens).toContainEqual({ token: "type.identifier", value: "LruNode" });
    expect(tokens).toContainEqual({ token: "delimiter", value: "<" });
    expect(tokens).toContainEqual({ token: "delimiter", value: ">" });
    expect(tokens).toContainEqual({ token: "type.identifier", value: "K" });
    expect(tokens).toContainEqual({ token: "type.identifier", value: "V" });
  });

  test("tokenizes generic parameters in struct and function headers", () => {
    const structTokens = tokenizeLine(provider, "struct LruNode<K, V> {");
    expect(structTokens).toContainEqual({ token: "type.identifier", value: "LruNode" });
    expect(structTokens).toContainEqual({ token: "delimiter", value: "<" });
    expect(structTokens).toContainEqual({ token: "type.identifier", value: "K" });
    expect(structTokens).toContainEqual({ token: "type.identifier", value: "V" });
    expect(structTokens).toContainEqual({ token: "delimiter", value: ">" });

    const functionTokens = tokenizeLine(provider, "fn append_new_node<K, V>(nodes, head) {");
    expect(functionTokens).toContainEqual({ token: "function", value: "append_new_node" });
    expect(functionTokens).toContainEqual({ token: "delimiter", value: "<" });
    expect(functionTokens).toContainEqual({ token: "type.identifier", value: "K" });
    expect(functionTokens).toContainEqual({ token: "type.identifier", value: "V" });
    expect(functionTokens).toContainEqual({ token: "delimiter", value: ">" });
  });
});
