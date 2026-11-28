import fs from 'node:fs';

const root = new URL('../../', import.meta.url);
const rustConsumers = [
  'src-tauri/src/service/mod.rs',
  'src-tauri/src/service/snapshot.rs',
  'src-tauri/src/settings/mod.rs',
  'src-tauri/src/settings/normalize.rs',
  'src-tauri/src/tray_presentation.rs',
  'src-tauri/src/pacing.rs',
  'src-tauri/src/notifications.rs',
  'src-tauri/src/commands/bootstrap.rs',
  'src-tauri/src/commands/settings.rs',
];
const frontendConsumers = [
  'src/App.svelte',
  'src/lib/CustomizeProviderDetail.svelte',
  'src/lib/CustomizeProviderList.svelte',
  'src/lib/Dashboard.svelte',
  'src/lib/MetricRenderer.svelte',
  'src/lib/StatusMetric.svelte',
  'src/lib/TotalSpend.svelte',
  'src/lib/metrics.ts',
  'src/lib/shareCard.ts',
];
const providerLiteral =
  /["'](?:claude|codex|cursor|antigravity|copilot|devin|grok|opencode|openrouter|zai|kimi|minimax|sub2api)["']/;
const cfgTestAttr = /^#\[\s*cfg\s*\(\s*test\s*\)\s*\]/;

function skipLineComment(source, pos) {
  while (pos < source.length && source[pos] !== '\n') pos++;
  return pos;
}

function skipBlockComment(source, pos) {
  let depth = 1;
  pos += 2;
  while (pos < source.length && depth > 0) {
    if (source[pos] === '/' && source[pos + 1] === '*') {
      depth++;
      pos += 2;
    } else if (source[pos] === '*' && source[pos + 1] === '/') {
      depth--;
      pos += 2;
    } else {
      pos++;
    }
  }
  return pos;
}

function skipQuotedString(source, pos) {
  pos++;
  while (pos < source.length) {
    if (source[pos] === '\\') pos += 2;
    else if (source[pos] === '"') return pos + 1;
    else pos++;
  }
  return pos;
}

function matchRawStringEnd(source, pos) {
  let cursor = pos;
  if (source[cursor] === 'b') cursor++;
  if (source[cursor] !== 'r') return -1;
  cursor++;
  let hashes = 0;
  while (cursor < source.length && source[cursor] === '#') {
    hashes++;
    cursor++;
  }
  if (source[cursor] !== '"') return -1;
  cursor++;
  const closing = `"${'#'.repeat(hashes)}`;
  const found = source.indexOf(closing, cursor);
  return found === -1 ? source.length : found + closing.length;
}

function skipWhitespaceCommentsAndAttrs(source, pos) {
  while (pos < source.length) {
    if (/\s/.test(source[pos])) {
      pos++;
      continue;
    }
    if (source[pos] === '/' && source[pos + 1] === '/') {
      pos = skipLineComment(source, pos);
      continue;
    }
    if (source[pos] === '/' && source[pos + 1] === '*') {
      pos = skipBlockComment(source, pos);
      continue;
    }
    if (source[pos] === '#' && source[pos + 1] === '[') {
      let depth = 0;
      pos++;
      while (pos < source.length) {
        if (source[pos] === '[') {
          depth++;
          pos++;
        } else if (source[pos] === ']') {
          depth--;
          pos++;
          if (depth === 0) break;
        } else if (source[pos] === '"') {
          pos = skipQuotedString(source, pos);
        } else {
          pos++;
        }
      }
      continue;
    }
    break;
  }
  return pos;
}

function skipRustItem(source, pos) {
  pos = skipWhitespaceCommentsAndAttrs(source, pos);
  let braceDepth = 0;
  let parenDepth = 0;
  let bracketDepth = 0;
  while (pos < source.length) {
    if (source[pos] === '/' && source[pos + 1] === '/') {
      pos = skipLineComment(source, pos);
      continue;
    }
    if (source[pos] === '/' && source[pos + 1] === '*') {
      pos = skipBlockComment(source, pos);
      continue;
    }
    const rawEnd = matchRawStringEnd(source, pos);
    if (rawEnd !== -1) {
      pos = rawEnd;
      continue;
    }
    if (source[pos] === '"') {
      pos = skipQuotedString(source, pos);
      continue;
    }
    if (source[pos] === "'") {
      if (source[pos + 1] === '\\') {
        pos += 2;
        while (pos < source.length && source[pos] !== "'") pos++;
        if (pos < source.length) pos++;
        continue;
      }
      if (pos + 2 < source.length && source[pos + 2] === "'") {
        pos += 3;
        continue;
      }
    }
    const ch = source[pos++];
    if (ch === '(') parenDepth++;
    else if (ch === ')') parenDepth = Math.max(0, parenDepth - 1);
    else if (ch === '[') bracketDepth++;
    else if (ch === ']') bracketDepth = Math.max(0, bracketDepth - 1);
    else if (ch === '{') braceDepth++;
    else if (ch === '}') {
      braceDepth--;
      if (braceDepth === 0 && parenDepth === 0 && bracketDepth === 0) return pos;
    } else if (ch === ';' && braceDepth === 0 && parenDepth === 0 && bracketDepth === 0) {
      return pos;
    }
  }
  return pos;
}

function stripCfgTestItems(source) {
  let out = '';
  let pos = 0;
  while (pos < source.length) {
    if (source[pos] === '/' && source[pos + 1] === '/') {
      const end = skipLineComment(source, pos);
      out += source.slice(pos, end);
      pos = end;
      continue;
    }
    if (source[pos] === '/' && source[pos + 1] === '*') {
      const end = skipBlockComment(source, pos);
      out += source.slice(pos, end);
      pos = end;
      continue;
    }
    const rawEnd = matchRawStringEnd(source, pos);
    if (rawEnd !== -1) {
      out += source.slice(pos, rawEnd);
      pos = rawEnd;
      continue;
    }
    if (source[pos] === '"') {
      const end = skipQuotedString(source, pos);
      out += source.slice(pos, end);
      pos = end;
      continue;
    }
    if (source[pos] === '#' && source[pos + 1] === '[') {
      const match = cfgTestAttr.exec(source.slice(pos));
      if (match) {
        pos = skipRustItem(source, pos + match[0].length);
        continue;
      }
    }
    out += source[pos++];
  }
  return out;
}

for (const file of rustConsumers) {
  const source = stripCfgTestItems(fs.readFileSync(new URL(file, root), 'utf8'));
  if (providerLiteral.test(source)) {
    throw new Error(`${file} contains provider identity outside the provider registry.`);
  }
  if (/ends_with\(\s*["']\./.test(source)) {
    throw new Error(`${file} infers metric behavior from an id suffix.`);
  }
}

for (const file of frontendConsumers) {
  const source = fs.readFileSync(new URL(file, root), 'utf8');
  if (providerLiteral.test(source)) {
    throw new Error(`${file} contains provider identity outside the visual asset registry.`);
  }
  if (/endsWith\(\s*["']\./.test(source)) {
    throw new Error(`${file} infers metric behavior from an id suffix.`);
  }
}

const providerRuntime = fs.readFileSync(new URL('src-tauri/src/providers/mod.rs', root), 'utf8');
if (!/fn definition\(&self\) -> ProviderDefinition;/.test(providerRuntime)) {
  throw new Error('UsageProvider does not own its ProviderDefinition.');
}

const bootstrap = fs.readFileSync(new URL('src-tauri/src/commands/bootstrap.rs', root), 'utf8');
if (!/pub catalog: ProviderCatalog/.test(bootstrap)) {
  throw new Error('BootstrapState does not expose the provider catalog.');
}

const metrics = fs.readFileSync(new URL('src/lib/metrics.ts', root), 'utf8');
if (/metricDefinitions\s*[:=]/.test(metrics)) {
  throw new Error('Frontend metrics must come from the backend provider catalog.');
}

const iconRegistry = fs.readFileSync(new URL('src/lib/providerIconPaths.ts', root), 'utf8');
if (/\?\?\s*codex/.test(iconRegistry)) {
  throw new Error('Unknown providers must not silently render the Codex icon.');
}

const compositionRoot = fs.readFileSync(new URL('src-tauri/src/lib.rs', root), 'utf8');
if (/\.has_local_credentials\(\)/.test(compositionRoot)) {
  throw new Error('Tauri setup must not synchronously probe provider credentials.');
}
const runtimeBlock = compositionRoot.match(/let mut providers\s*=\s*vec!\[([\s\S]*?)\];/)?.[1];
if (!runtimeBlock) {
  throw new Error('Tauri setup does not expose the provider runtime list.');
}
const runtimeOrder = [...runtimeBlock.matchAll(/Arc::new\((\w+Provider)::new\b/g)].map(
  ([, provider]) => provider,
);
if (/providers\.extend\(sub2api\.runtimes\(\)\)/.test(compositionRoot))
  runtimeOrder.push('Sub2ApiProvider');
const expectedRuntimeOrder = [
  'ClaudeProvider',
  'CodexProvider',
  'CursorProvider',
  'AntigravityProvider',
  'CopilotProvider',
  'DevinProvider',
  'GrokProvider',
  'OpenCodeProvider',
  'OpenRouterProvider',
  'ZaiProvider',
  'KimiProvider',
  'MiniMaxProvider',
  'Sub2ApiProvider',
];
if (runtimeOrder.join(',') !== expectedRuntimeOrder.join(',')) {
  throw new Error(
    `Tauri provider runtime order is incomplete: expected ${expectedRuntimeOrder.join(', ')}, got ${runtimeOrder.join(', ')}`,
  );
}

const credentialDetection = fs.readFileSync(
  new URL('src-tauri/src/providers/detection.rs', root),
  'utf8',
);
if (!/spawn_blocking/.test(credentialDetection) || !/registry\.runtime/.test(credentialDetection)) {
  throw new Error('Credential detection must fan out registry runtimes on blocking workers.');
}

const providerCommands = fs.readFileSync(
  new URL('src-tauri/src/commands/provider.rs', root),
  'utf8',
);
if (!/registry\s*\.definition\(provider_id\)/s.test(providerCommands)) {
  throw new Error('Provider links must resolve from registry metadata.');
}
if (/pub fn open_provider_link[\s\S]*?url:\s*String/.test(providerCommands)) {
  throw new Error('The provider-link command must not accept an arbitrary URL.');
}

console.log(
  `${rustConsumers.length + frontendConsumers.length} provider consumers use registry metadata and ${runtimeOrder.length} runtimes keep the canonical order.`,
);
