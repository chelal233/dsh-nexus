import assert from "node:assert/strict";
import test from "node:test";

import { translateForTest } from "../src/i18n.ts";

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
