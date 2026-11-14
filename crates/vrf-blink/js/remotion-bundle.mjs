import { createRequire } from 'node:module';
import { writeFileSync } from 'node:fs';
import { resolve } from 'node:path';

const [projectDir, entry, outDir, publicDir, depsFile] = process.argv.slice(2).map((p) => resolve(p));
process.chdir(projectDir);
const { bundle } = createRequire(`${projectDir}/package.json`)('@remotion/bundler');
const options = { entryPoint: entry, outDir, enableCaching: false };
const files = new Set();
if (publicDir) {
  options.publicDir = publicDir;
  options.webpackOverride = (config) => ({
    ...config,
    plugins: [
      ...(config.plugins ?? []),
      {
        apply(compiler) {
          compiler.hooks.afterEmit.tap('vrf-inputs', (compilation) => {
            for (const file of compilation.fileDependencies) files.add(file);
          });
        },
      },
    ],
  });
}
const out = await bundle(options);
if (depsFile) {
  const inputs = [...files].filter((file) => !file.split(/[\\/]/).includes('node_modules')).map((file) => resolve(file));
  writeFileSync(depsFile, JSON.stringify([...new Set(inputs)].sort()));
}
process.stdout.write(`${out}\n`);
