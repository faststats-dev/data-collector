import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import {execFileSync} from 'node:child_process';
import {createRequire} from 'node:module';
import {rollup} from 'rollup';
import {rolldown} from 'rolldown';
import webpack from 'webpack';
import {SourceMapConsumer} from 'source-map';
import assert from 'node:assert/strict';

const require = createRequire(import.meta.url);
const Metro = require('metro');
const {getDefaultConfig, mergeConfig} = require('metro-config');
const {composeSourceMaps} = require('metro-source-map');
const {createContext} = require('metro-symbolicate/private/Symbolication');
const MetroConsumer = require('metro-source-map').Consumer;
const root = process.cwd();
const work = path.join(root, 'work');
fs.mkdirSync(work, {recursive: true});
const write = (dir, name, value) => fs.writeFileSync(path.join(dir, name), value);
function output(name) {
  const dir = path.join('/output', name);
  fs.mkdirSync(dir, {recursive: true});
  return dir;
}
function normalize(map) {
  if (map.sources) map.sources = map.sources.map(source => source.replaceAll(root + '/', '').replace(/^\.\.\/src\//, 'src/'));
  if (map.sections) map.sections.forEach(section => normalize(section.map));
  return map;
}
function crash(code) {
  try { vm.runInNewContext(code, {}, {filename: 'bundle.js'}); }
  catch (error) {
    const frames = error.stack.split('\n').filter(line => /^\s+at .*bundle\.js:\d+:\d+\)?$/.test(line));
    assert(frames.length >= 2, error.stack);
    return 'Error: fixture crash\n' + frames.join('\n') + '\n';
  }
  throw new Error('Fixture did not throw');
}
async function browserFixture(name, code, rawMap, metro = false) {
  const dir = output(name);
  const map = normalize(rawMap);
  const input = crash(code);
  const consumer = await new SourceMapConsumer(map);
  const context = metro ? createContext(MetroConsumer, map, {inputColumnStart: 1, outputColumnStart: 1}) : null;
  const expected = input.split('\n').map(line => {
    const match = line.match(/bundle\.js:(\d+):(\d+)/);
    if (!match) return line;
    const position = context
      ? context.getOriginalPositionFor(+match[1], +match[2])
      : consumer.originalPositionFor({line: +match[1], column: +match[2] - 1});
    if (!position.source || !position.line) return line;
    const location = `${position.source}:${position.line}:${position.column + (metro ? 0 : 1)}`;
    if (position.name) return `    at ${position.name} (${location})`;
    return line.replace(/bundle\.js:\d+:\d+/, location);
  }).join('\n');
  write(dir, 'bundle.js', code);
  write(dir, 'bundle.js.map', JSON.stringify(map, null, 2) + '\n');
  write(dir, 'input.txt', input);
  write(dir, 'expected.txt', expected);
  const firefox = trace => trace.replace(/^    at (?:(.*?) \()?([^ ()]+:\d+:\d+)\)?$/gm, (_, name, location) => `${name ?? ''}@${location}`);
  write(dir, 'firefox-input.txt', firefox(input));
  write(dir, 'firefox-expected.txt', firefox(expected));
  write(dir, 'generator.json', JSON.stringify({tool: name, node: process.version, oracle: metro ? 'metro-symbolicate' : 'source-map', versions: JSON.parse(fs.readFileSync('package.json')).dependencies}, null, 2) + '\n');
  consumer.destroy();
  return map;
}

for (const [name, build] of [['rollup', rollup], ['rolldown', rolldown]]) {
  const bundle = await build({input: 'src/index.js'});
  const result = await bundle.generate({format: 'iife', sourcemap: true, file: 'bundle.js', ...(name === 'rolldown' ? {minify: true} : {})});
  const code = result.output[0].code;
  const map = JSON.parse(result.output[0].map.toString());
  await browserFixture(name, code, map);
  if (name === 'rollup') {
    await browserFixture('rollup_root', code, {...map, sourceRoot: '/project/'});
  }
  await browserFixture(`${name}_indexed`, '// prelude\n/* shifted */' + code, {
    version: 3,
    sections: [
      {offset: {line: 0, column: 0}, map: {version: 3, sources: [], names: [], mappings: ''}},
      {offset: {line: 1, column: 13}, map},
    ],
  });
  await bundle.close();
}
const typed = await rolldown({input: 'src/typed.ts'});
const typedOutput = await typed.generate({format: 'iife', sourcemap: true, file: 'bundle.js', minify: true});
await browserFixture('rolldown_typescript', typedOutput.output[0].code, JSON.parse(typedOutput.output[0].map.toString()));
await typed.close();

for (const [name, mode, devtool] of [
  ['webpack', 'production', 'source-map'],
  ['webpack_development', 'development', 'source-map'],
  ['webpack_nosources', 'production', 'nosources-source-map'],
]) {
  const compiler = webpack({
    mode, entry: path.join(root, 'src/index.js'), devtool,
    output: {path: path.join(work, name), filename: 'bundle.js', devtoolModuleFilenameTemplate: 'webpack:///./[resource-path]'},
  });
  await new Promise((resolve, reject) => compiler.run((error, stats) => {
    compiler.close(() => {});
    if (error || stats.hasErrors()) reject(error ?? new Error(stats.toString())); else resolve();
  }));
  await browserFixture(name, fs.readFileSync(path.join(work, name, 'bundle.js'), 'utf8'), JSON.parse(fs.readFileSync(path.join(work, name, 'bundle.js.map'))));
}

// Metro's normal CommonJS pipeline, using the same application logic as the web builds.
const metroSource = path.join(root, 'metro-src');
fs.mkdirSync(metroSource, {recursive: true});
fs.writeFileSync(path.join(metroSource, 'crash.js'), fs.readFileSync('src/crash.js', 'utf8').replaceAll('export function ', 'function ') + '\nexports.run = run;\n');
fs.writeFileSync(path.join(metroSource, 'index.js'), "const {run} = require('./crash');\nrun();\n");
const config = mergeConfig(await getDefaultConfig(root), {
  projectRoot: root, watchFolders: [root], maxWorkers: 1,
  resolver: {useWatchman: false},
  reporter: {update() {}},
});
const metroOut = path.join(work, 'metro.js');
await Metro.runBuild(config, {entry: 'metro-src/index.js', platform: 'android', dev: false, minify: true, out: metroOut, sourceMap: true, sourceMapUrl: 'bundle.js.map', sourceMapOut: metroOut + '.map'});
const metroMap = await browserFixture('metro', fs.readFileSync(metroOut, 'utf8'), JSON.parse(fs.readFileSync(metroOut + '.map')), true);

// Probe compiler-generated Hermes bytecode offsets.
const hermesOut = path.join(work, 'bundle.hbc');
execFileSync('node_modules/hermes-compiler/hermesc/linux64-bin/hermesc', ['-O', '-emit-binary', '-output-source-map', '-out', hermesOut, metroOut]);
const hermesMap = JSON.parse(fs.readFileSync(hermesOut + '.map'));
const composed = composeSourceMaps([metroMap, hermesMap]);
const context = createContext(MetroConsumer, composed, {inputColumnStart: 0, outputColumnStart: 1});
const consumer = await new SourceMapConsumer(composed);
const probes = [];
consumer.eachMapping(mapping => {
  if (mapping.source?.endsWith('metro-src/crash.js') && mapping.originalLine === 2 && probes.length < 3) {
    const position = context.getOriginalPositionFor(mapping.generatedLine, mapping.generatedColumn);
    assert(position.name && position.source);
    probes.push({line: mapping.generatedLine, offset: mapping.generatedColumn, ...position});
  }
});
assert(probes.length > 0, 'No Hermes throw positions');
const dir = output('hermes');
let input = 'Error: fixture crash\n';
let expected = 'Error: fixture crash\n';
for (const probe of probes) {
  const location = `${probe.source}:${probe.line}:${probe.column}`;
  input += `    at a (address at index.android.bundle:1:${probe.offset})\n`;
  expected += `    at ${probe.name} (${location})\n`;
  input += `a@1:${probe.offset}\n`;
  expected += `${probe.name}@${location}\n`;
}
write(dir, 'bundle.js.map', JSON.stringify(composed, null, 2) + '\n');
write(dir, 'compiler.js.map', JSON.stringify(hermesMap, null, 2) + '\n');
write(dir, 'input.txt', input);
write(dir, 'expected.txt', expected);
write(dir, 'generator.json', JSON.stringify({tool: 'hermes-compiler', version: '250829098.0.19', oracle: 'metro-symbolicate 0.87.1', kind: 'compiler-derived bytecode position probes, not a captured device crash'}, null, 2) + '\n');
consumer.destroy();
console.log('Generated JavaScript, TypeScript, indexed-map, Metro and Hermes fixtures');
