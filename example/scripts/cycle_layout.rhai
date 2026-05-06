// Cycle through the layouts bound to the Mango switch-layout shortcut.
let cycle = ["Tile", "Scroller"];

let current = get("layout_name");
let next_idx = 0;

// Find current position in cycle and get next
let len = cycle.len();
for i in 0..len {
    if cycle[i] == current {
        next_idx = (i + 1) % len;
        break;
    }
}

set("layout_name", cycle[next_idx]);
