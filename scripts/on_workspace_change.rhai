// Called on every workspace change
// Logs the current workspace and layout

let ws = get("workspace");
let layout = get("layout");
let monitor = get("monitor");

log("Workspace changed to " + ws + " on " + monitor);
log("Current layout: " + layout);

// Return success
true
