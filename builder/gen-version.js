const fs = require('fs');
const path = require('path');

// The plugin twin of app/builder/gen-version.js: package.json is the single
// source of truth, and every plugin reports whatever lands in version.rs.
const root = path.join(__dirname, '..');
const { version } = JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8'));

const content = `// Generated from package.json by builder/gen-version.js. Do not edit by hand.
//
// A macro rather than a const so \`concat!\` can put it next to a NUL byte, which
// is what the C ABI hands back from ic_plugin_version.
macro_rules! plugins_version {
    () => {
        "${version}"
    };
}
`;

const out = path.join(root, 'version.rs');
fs.writeFileSync(out, content, 'utf8');
console.log(`Generated version.rs -> v${version}`);
