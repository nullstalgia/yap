fn main() -> color_eyre::Result<()> {
    yap::run()
}

// Sooner TODOs:
// deduplication of code for defmt and logging defmt
// SignPath for release binaries
// ARM builds in releases
// main menu cursor logic, if on a device, stay on it, otherwise on lower menu index
// Set defmt mode based on ELF's mode on user's load (but not loading last on startup)
// command palette
// maybe macro to make commands?
// clear buffer command
// fully optional tls/openssl

// Defaults based on flavor

// customizable scrolling line snippet

// log timstamps after resync are borked?

// for new text pipeline
// maybe ropes?
// can resume ansi state
// can omit certain strings (invalid->escaped bytes/line endings?)

// text wrapping on + kitty resizing a big buffer = pain

// centering of port info on smaller terminal

// build.rs to get version of espflash and defmt

// General TODOs:
// Mouse select in line mode?
//   and in Hex view to show make finding bytes easier
// Notification History
// Max buffer size (currently unlimited)

// Far future TODOs:
// Serial Forwarding + Loopback support
// TCP Socket support

// Unimportant but neat TODOs:
// Click on yap bigtext to swap style
// Click on port info in terminal menu to invert style?
