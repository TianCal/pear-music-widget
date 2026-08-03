'use strict';

/**
 * electron-builder afterPack hook.
 *
 * With `identity: null` electron-builder skips signing entirely, which leaves
 * the bundle's seal broken ("code has no resources but signature indicates they
 * must be present") and makes Gatekeeper refuse the app on another machine.
 * Apple Silicon requires at least an ad-hoc signature, so apply one here:
 * nested code first, then the outer bundle.
 */

const { execFileSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

const sign = (target) =>
  execFileSync('codesign', ['--force', '--timestamp=none', '--sign', '-', target], {
    stdio: 'pipe',
  });

exports.default = async (context) => {
  if (context.electronPlatformName !== 'darwin') return;

  const appPath = path.join(
    context.appOutDir,
    `${context.packager.appInfo.productFilename}.app`,
  );
  const frameworks = path.join(appPath, 'Contents', 'Frameworks');

  // Helpers and frameworks must be sealed before the bundle that contains them.
  if (fs.existsSync(frameworks)) {
    for (const entry of fs.readdirSync(frameworks)) {
      sign(path.join(frameworks, entry));
    }
  }
  sign(appPath);

  execFileSync('codesign', ['--verify', '--deep', '--strict', appPath], { stdio: 'pipe' });
  console.log(`  • ad-hoc signed and verified  ${path.basename(appPath)}`);
};
