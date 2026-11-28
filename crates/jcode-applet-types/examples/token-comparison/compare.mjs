import { getEncoding } from "js-tiktoken";

const input = await Bun.stdin.text();
const rows = JSON.parse(input);
const encoding = getEncoding("cl100k_base");

// Recursively sort keys, then serialize without insignificant whitespace.
function canonical(value) {
  if (Array.isArray(value)) return value.map(canonical);
  if (value !== null && typeof value === "object") {
    return Object.fromEntries(Object.keys(value).sort().map((key) => [key, canonical(value[key])]));
  }
  return value;
}

console.log("| Macro | Language | Native | Express | Saved | Saved % |");
console.log("| --- | --- | ---: | ---: | ---: | ---: |");
for (const row of rows) {
  if (row.native.intent !== row.express.intent || row.native.action !== row.express.action) {
    throw new Error("Tool call metadata differs");
  }
  const native = encoding.encode(JSON.stringify(canonical(row.native))).length;
  const express = encoding.encode(JSON.stringify(canonical(row.express))).length;
  const saved = native - express;
  console.log(`| ${row.macro} | ${row.language} | ${native} | ${express} | ${saved} | ${(100 * saved / native).toFixed(1)}% |`);
}
