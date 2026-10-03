// Generated from package.json by builder/gen-version.js. Do not edit by hand.
//
// A macro rather than a const so `concat!` can put it next to a NUL byte, which
// is what the C ABI hands back from ic_plugin_version.
macro_rules! plugins_version {
    () => {
        "0.1.0"
    };
}
