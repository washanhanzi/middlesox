// Called when window count changes on a workspace

let ws = get("workspace");
let window_count = get("window_count");

log("Window count on workspace " + ws + ": " + window_count);

// Example: Auto-switch to float layout when only 1 window
if window_count == 1 {
    let current = get("layout");
    if current != "float" {
        log("Single window detected, consider float layout");
        // Uncomment to auto-switch:
        // set("layout", "float");
    }
}

true
