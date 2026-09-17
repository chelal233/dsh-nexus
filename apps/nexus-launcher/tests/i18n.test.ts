import assert from "node:assert/strict";
import test from "node:test";

import { localeFromLanguages, translateForTest, translationKeysForTest } from "../src/i18n.ts";

test("system language preferences select a supported language before falling back to English", () => {
  assert.equal(localeFromLanguages(["zh-TW", "en-US"]), "zh");
  assert.equal(localeFromLanguages(["en-GB", "zh-CN"]), "en");
  assert.equal(localeFromLanguages(["ja-JP", "zh_CN"]), "zh");
  assert.equal(localeFromLanguages(["fr-FR"]), "en");
  assert.equal(localeFromLanguages([]), "en");
});

test("supports English and Simplified Chinese translations with interpolation", () => {
  assert.equal(translateForTest("en", "Overview"), "Overview");
  assert.equal(translateForTest("zh", "Overview"), "概览");
  assert.equal(
    translateForTest("zh", "{count} profiles available", { count: 3 }),
    "3 个可用配置档",
  );
  assert.equal(
    translateForTest("zh", "Harness is not configured. Open Settings to configure it."),
    "Harness 尚未配置，请打开设置完成配置。",
  );
});

test("English and Simplified Chinese dictionaries expose the same keys", () => {
  assert.deepEqual(translationKeysForTest("zh"), translationKeysForTest("en"));
});
