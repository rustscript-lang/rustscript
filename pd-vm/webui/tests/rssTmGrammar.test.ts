import { describe, expect, test } from "bun:test";

import grammar from "../src/monaco/rss.tmLanguage.json";
import controllerGrammar from "../../../pd-controller/webui/src/app/monaco/rss.tmLanguage.json";
import extensionGrammar from "../../../.vscode/rss-language-extension/syntaxes/rss.tmLanguage.json";

type GrammarPattern = {
  begin?: string;
  include?: string;
  match?: string;
  patterns?: GrammarPattern[];
};

type GrammarSection = {
  patterns?: GrammarPattern[];
};

type Grammar = {
  patterns?: GrammarPattern[];
  repository?: Record<string, GrammarSection>;
};

function repoPattern(name: string, index = 0): GrammarPattern {
  const section = (grammar as Grammar).repository?.[name];
  if (!section?.patterns?.[index]) {
    throw new Error(`missing grammar pattern ${name}[${index}]`);
  }
  return section.patterns[index];
}

describe("RustScript TextMate grammar", () => {
  test("does not apply root type-annotation rules outside type contexts", () => {
    const rootIncludes = ((grammar as Grammar).patterns ?? [])
      .map((pattern) => pattern.include)
      .filter((value): value is string => typeof value === "string");

    expect(rootIncludes).not.toContain("#type-annotations");
  });

  test("uses contextual begin/end rules for typed lets and function signatures", () => {
    const typedLet = repoPattern("variable-declaration", 0);
    const functionDecl = repoPattern("function-declaration", 0);
    const functionParam = repoPattern("function-parameter", 0);

    expect(typedLet.begin).toBeTruthy();
    expect(functionDecl.begin).toBeTruthy();
    expect(functionParam.begin).toBeTruthy();

    expect(new RegExp(typedLet.begin ?? "").test("let mut detached: ")).toBe(true);
    expect(new RegExp(functionParam.begin ?? "").test("value: ")).toBe(true);
    expect(new RegExp(typedLet.begin ?? "").test("score: closure_value,")).toBe(false);
  });

  test("lets declaration headers reuse generic-type-arguments for K/V highlighting", () => {
    const structDecl = repoPattern("struct-declaration", 0);
    const pubFunctionDecl = repoPattern("function-declaration", 0);
    const functionDecl = repoPattern("function-declaration", 1);

    const structIncludes = (structDecl.patterns ?? [])
      .map((pattern) => pattern.include)
      .filter((value): value is string => typeof value === "string");
    const pubFunctionIncludes = (pubFunctionDecl.patterns ?? [])
      .map((pattern) => pattern.include)
      .filter((value): value is string => typeof value === "string");
    const functionIncludes = (functionDecl.patterns ?? [])
      .map((pattern) => pattern.include)
      .filter((value): value is string => typeof value === "string");

    expect(new RegExp(structDecl.begin ?? "").test("struct LruNode")).toBe(true);
    expect(new RegExp(pubFunctionDecl.begin ?? "").test("pub fn append_new_node")).toBe(true);
    expect(new RegExp(functionDecl.begin ?? "").test("fn append_new_node")).toBe(true);

    expect(structIncludes).toContain("#generic-type-arguments");
    expect(pubFunctionIncludes).toContain("#generic-type-arguments");
    expect(functionIncludes).toContain("#generic-type-arguments");
  });

  test("keeps synced grammar copies aligned", () => {
    expect(extensionGrammar).toEqual(grammar);
    expect(controllerGrammar).toEqual(grammar);
  });
});
