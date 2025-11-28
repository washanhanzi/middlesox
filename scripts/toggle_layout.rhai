// Toggle layout between "master" and "grid"
// This script is triggered when moving from workspace 1 to workspace 2

let current_layout = get("layout");
log("Current layout: " + current_layout);

if current_layout == "master" {
    set("layout", "grid");
    log("Switched to grid layout");
} else {
    set("layout", "master");
    log("Switched to master layout");
}

// Return the new layout
get("layout")
