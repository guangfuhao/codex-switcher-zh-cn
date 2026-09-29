import assert from "node:assert/strict";
import test from "node:test";
import {
  parseCodexClosePreference,
  rememberedCodexClosePreference,
  canCloseCodexDesktop,
} from "../src/lib/codexClosePreference.ts";

test("missing and invalid close preferences default to graceful close", () => {
  for (const value of [null, "", "true", "always", "invalid"]) {
    assert.equal(parseCodexClosePreference(value), "graceful");
  }
});

test("legacy force preferences migrate to graceful while ask remains available", () => {
  assert.equal(parseCodexClosePreference("graceful"), "graceful");
  assert.equal(parseCodexClosePreference("force"), "graceful");
  assert.equal(parseCodexClosePreference("ask"), "ask");
});

test("remembered selections can never restore force close", () => {
  assert.equal(rememberedCodexClosePreference(true, true), "graceful");
  assert.equal(rememberedCodexClosePreference(false, true), "graceful");
  assert.equal(rememberedCodexClosePreference(true, false), "ask");
});

test("closing is available only for classified desktop sessions without external sessions", () => {
  assert.equal(canCloseCodexDesktop({ count: 1, external_count: 0 }), true);
  assert.equal(canCloseCodexDesktop({ count: 2, external_count: 0 }), true);
  assert.equal(canCloseCodexDesktop({ count: 2, external_count: 1 }), false);
  assert.equal(canCloseCodexDesktop({ count: 1, external_count: 1 }), false);
  assert.equal(canCloseCodexDesktop({ count: 0, external_count: 0 }), false);
  assert.equal(canCloseCodexDesktop(null), false);
});

test("missing or invalid process classification never enables desktop closing", () => {
  for (const info of [
    { count: 1 },
    { count: 1, external_count: -1 },
    { count: 1, external_count: Number.NaN },
    { count: -1, external_count: 0 },
    { count: 0.5, external_count: 0 },
    { count: Number.NaN, external_count: 0 },
  ]) {
    assert.equal(canCloseCodexDesktop(info as { count: number; external_count: number }), false);
  }
});
