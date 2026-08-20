// Bundle the extension into one file. VS Code loads `dist/extension.js` on activation, and a
// single file loads faster than walking a `node_modules` tree — and keeps it out of the VSIX.
import * as esbuild from 'esbuild';

const watch = process.argv.includes('--watch');

const options = {
  entryPoints: ['src/extension.ts'],
  bundle: true,
  outfile: 'dist/extension.js',
  // `vscode` is provided by the extension host at runtime and must never be bundled.
  external: ['vscode'],
  format: 'cjs',
  platform: 'node',
  // The oldest Node any supported VS Code ships with: 1.108 runs Electron 39, which is Node 22.
  target: 'node22',
  sourcemap: true,
  minify: !watch,
  logLevel: 'info',
};

if (watch) {
  const ctx = await esbuild.context(options);
  await ctx.watch();
} else {
  await esbuild.build(options);
}
