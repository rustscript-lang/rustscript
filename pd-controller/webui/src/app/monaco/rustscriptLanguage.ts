import type * as Monaco from "monaco-editor";

import languageConfiguration from "@/app/monaco/rss.language-configuration.json";
import tmLanguage from "@/app/monaco/rss.tmLanguage.json";

type TmPattern = {
  name?: string;
  match?: string;
  begin?: string;
  end?: string;
  patterns?: TmPattern[];
};

type TmSection = {
  patterns?: TmPattern[];
  match?: string;
  begin?: string;
  end?: string;
};

type TmGrammar = {
  repository?: Record<string, TmSection>;
};

const FALLBACK_BLOCK_COMMENT_BEGIN = "/\\*";
const FALLBACK_BLOCK_COMMENT_END = "\\*/";
const FALLBACK_LINE_COMMENT = "//.*$";
const FALLBACK_STRING_BEGIN = "\"";
const FALLBACK_STRING_END = "\"";
const FALLBACK_STRING_ESCAPE = "\\\\(?:[nrt\\\\\"0])";
const FALLBACK_NUMBERS = "\\b(?:\\d+\\.\\d+|\\d+)\\b";
const FALLBACK_OPERATORS = "=>|==|!=|&&|\\|\\||=|\\+|-|\\*|/|%|<|>|!|\\?";
const IDENT = "[A-Za-z_][A-Za-z0-9_]*";
const PATH_IDENT = `(?:self|super|crate|${IDENT})`;
const PATH = `(?:${PATH_IDENT})(?:::(?:${PATH_IDENT}))*`;
const PATH_CALL = `${IDENT}(?:(?:\\s*::\\s*)${IDENT})*`;

let rustScriptLanguageRegistered = false;

function section(name: string): TmSection {
  const grammar = tmLanguage as TmGrammar;
  return grammar.repository?.[name] ?? {};
}

function sectionPatterns(name: string): TmPattern[] {
  return section(name).patterns ?? [];
}

function patternMatch(value: string | undefined, fallback: string): RegExp {
  try {
    return new RegExp(value && value.length > 0 ? value : fallback);
  } catch {
    return new RegExp(fallback);
  }
}

function sectionMatch(name: string, fallback: string): RegExp {
  return patternMatch(section(name).match, fallback);
}

function sectionPatternMatches(name: string): RegExp[] {
  return sectionPatterns(name)
    .map((entry) => entry.match)
    .filter((entry): entry is string => typeof entry === "string" && entry.length > 0)
    .map((entry) => patternMatch(entry, "$^"));
}

function findByName(name: string, needle: string): TmPattern | null {
  return sectionPatterns(name).find((entry) => entry.name?.includes(needle)) ?? null;
}

function beginPattern(entry: TmPattern | null, fallback: string): RegExp {
  return patternMatch(entry?.begin, fallback);
}

function endPattern(entry: TmPattern | null, fallback: string): RegExp {
  return patternMatch(entry?.end, fallback);
}

export function ensureRustScriptLanguage(monaco: typeof import("monaco-editor")): void {
  if (rustScriptLanguageRegistered) {
    return;
  }

  const languageId = "rustscript";
  if (!monaco.languages.getLanguages().some((item) => item.id === languageId)) {
    monaco.languages.register({ id: languageId });
  }

  const lineCommentPattern = beginPattern(
    findByName("comments", "comment.line"),
    FALLBACK_LINE_COMMENT,
  );
  const blockCommentBegin = beginPattern(
    findByName("comments", "comment.block"),
    FALLBACK_BLOCK_COMMENT_BEGIN,
  );
  const blockCommentEnd = endPattern(
    findByName("comments", "comment.block"),
    FALLBACK_BLOCK_COMMENT_END,
  );
  const stringBegin = beginPattern(
    section("double-quoted-string"),
    FALLBACK_STRING_BEGIN,
  );
  const stringEnd = endPattern(
    section("double-quoted-string"),
    FALLBACK_STRING_END,
  );
  const stringEscape = sectionPatternMatches("double-quoted-string")[0] ??
    patternMatch(FALLBACK_STRING_ESCAPE, FALLBACK_STRING_ESCAPE);

  const rootRules: Monaco.languages.IMonarchLanguageRule[] = [
    [lineCommentPattern, "comment"],
    [blockCommentBegin, "comment", "@blockComment"],
    [stringBegin, "string", "@string"],
    [
      new RegExp(`\\b(use)(\\s+)(${PATH})(\\s+)(as)(\\s+)(${IDENT})\\b`),
      ["keyword", "", "type.identifier", "", "keyword", "", "identifier"],
    ],
    [
      new RegExp(`\\b(use)(\\s+)(${PATH})(\\s*)(::)(\\s*)(\\{[^}\\n]*\\}|\\*)`),
      ["keyword", "", "type.identifier", "", "delimiter", "", "type.identifier"],
    ],
    [new RegExp(`\\b(use)(\\s+)(${PATH})\\b`), ["keyword", "", "type.identifier"]],
    [
      new RegExp(`\\b(pub)(\\s+)(fn)(\\s+)(${IDENT})\\s*(?=\\()`),
      ["keyword", "", "keyword", "", "function"],
    ],
    [new RegExp(`\\b(fn)(\\s+)(${IDENT})\\s*(?=\\()`), ["keyword", "", "function"]],
    [new RegExp(`\\b(let)(\\s+)(${IDENT})\\b`), ["keyword", "", "identifier"]],
    [
      new RegExp(`\\b(${IDENT})(\\s*)(::)(\\s*)(${PATH_CALL})(?=\\s*\\()`),
      ["type.identifier", "", "delimiter", "", "function"],
    ],
    [new RegExp(`\\b(${IDENT})(\\s*)(!)(?=\\s*\\()`), ["function", "", "operator"]],
    [
      /(\?\.|\.)(\s*)([A-Za-z_][A-Za-z0-9_]*)/,
      ["delimiter", "", "variable"],
    ],
    [
      /\b(?!pub\b|fn\b|let\b|for\b|if\b|else\b|match\b|while\b|break\b|continue\b|use\b|as\b|true\b|false\b|null\b)([A-Za-z_][A-Za-z0-9_]*)\s*(?=\()/,
      "function",
    ],
  ];

  rootRules.push([sectionMatch("booleans", "\\b(?:true|false)\\b"), "keyword"]);
  rootRules.push([sectionMatch("null-literal", "\\bnull\\b"), "keyword"]);
  rootRules.push([sectionMatch("numbers", FALLBACK_NUMBERS), "number"]);
  for (const keywordPattern of sectionPatternMatches("keywords")) {
    rootRules.push([keywordPattern, "keyword"]);
  }
  rootRules.push([sectionMatch("wildcard-pattern", "\\b_\\b"), "keyword"]);
  rootRules.push([sectionMatch("closure-pipes", "\\|"), "delimiter"]);
  rootRules.push([sectionMatch("operators", FALLBACK_OPERATORS), "operator"]);
  rootRules.push([sectionMatch("punctuation", "[(){}\\[\\],;:]"), "delimiter"]);

  monaco.languages.setMonarchTokensProvider(languageId, {
    tokenizer: {
      root: rootRules,
      blockComment: [
        [blockCommentEnd, "comment", "@pop"],
        [/./, "comment"],
      ],
      string: [
        [stringEscape, "string.escape"],
        [stringEnd, "string", "@pop"],
        [/[^\\"]+/, "string"],
        [/./, "string"],
      ],
    },
  });

  monaco.languages.setLanguageConfiguration(
    languageId,
    languageConfiguration as unknown as Monaco.languages.LanguageConfiguration,
  );

  rustScriptLanguageRegistered = true;
}
