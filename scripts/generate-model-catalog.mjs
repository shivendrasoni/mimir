#!/usr/bin/env node

import { readFile, writeFile } from "node:fs/promises";
import { resolve } from "node:path";

const [sourceArg, outputArg] = process.argv.slice(2);
if (!sourceArg || !outputArg) {
  throw new Error("usage: generate-model-catalog.mjs <models.generated.ts> <output.json>");
}

const source = await readFile(resolve(sourceArg), "utf8");
const lines = source.split(/\r?\n/);
const models = [];

function stringField(block, field) {
  const match = block.match(new RegExp(`^\\t\\t\\t${field}: \\"([^\\"]*)\\",$`, "m"));
  if (!match) throw new Error(`missing ${field} in model block`);
  return match[1];
}

function numberField(block, field) {
  const match = block.match(new RegExp(`^\\t\\t\\t${field}: (-?[0-9.eE+]+),$`, "m"));
  if (!match) throw new Error(`missing ${field} in model block`);
  return Number(match[1]);
}

function inlineJsonField(block, field) {
  const match = block.match(new RegExp(`^\\t\\t\\t${field}: (.+),$`, "m"));
  return match ? JSON.parse(match[1]) : undefined;
}

for (let index = 0; index < lines.length; index += 1) {
  const start = lines[index].match(/^\t\t"([^"]+)": \{$/);
  if (!start) continue;
  let end = index + 1;
  while (end < lines.length && !/^\t\t\} satisfies Model<"[^"]+">,$/.test(lines[end])) {
    end += 1;
  }
  if (end >= lines.length) continue;
  const apiFromMarker = lines[end].match(/Model<"([^"]+)">/)[1];
  const block = lines.slice(index + 1, end).join("\n");
  if (!/^\t\t\tid: /m.test(block)) continue;

  const costBlock = block.match(/^\t\t\tcost: \{\n([\s\S]*?)^\t\t\t\},$/m);
  if (!costBlock) throw new Error(`missing cost in ${start[1]}`);
  const cost = {};
  for (const field of ["input", "output", "cacheRead", "cacheWrite"]) {
    const match = costBlock[1].match(new RegExp(`^\\t\\t\\t\\t${field}: (-?[0-9.eE+]+),$`, "m"));
    if (!match) throw new Error(`missing cost.${field} in ${start[1]}`);
    cost[field] = Number(match[1]);
  }

  const model = {
    id: stringField(block, "id"),
    name: stringField(block, "name"),
    api: stringField(block, "api"),
    provider: stringField(block, "provider"),
    baseUrl: stringField(block, "baseUrl"),
    reasoning: block.match(/^\t\t\treasoning: (true|false),$/m)?.[1] === "true",
    input: inlineJsonField(block, "input"),
    cost,
    contextWindow: numberField(block, "contextWindow"),
    maxTokens: numberField(block, "maxTokens"),
  };
  if (model.api !== apiFromMarker) {
    throw new Error(`API marker mismatch for ${model.provider}/${model.id}`);
  }
  for (const field of ["thinkingLevelMap", "compat", "headers"]) {
    const value = inlineJsonField(block, field);
    if (value !== undefined) model[field] = value;
  }
  const featured = block.match(/^\t\t\tfeatured: (true|false),$/m);
  if (featured) model.featured = featured[1] === "true";
  if (start[1] !== model.id) {
    throw new Error(`model key mismatch: ${start[1]} != ${model.id}`);
  }
  models.push(model);
  index = end;
}

if (models.length < 100) {
  throw new Error(`parsed only ${models.length} models; refusing incomplete catalog`);
}

await writeFile(resolve(outputArg), `${JSON.stringify(models, null, 2)}\n`);
console.log(`wrote ${models.length} models to ${resolve(outputArg)}`);
