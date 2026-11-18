# ilex shell design source

Source material for the future ilex shell; this directory is not installed
anywhere and is not an image or installer input. `design-system.toml` records
the export's provenance and description, not a selectable configuration.

`tokens.json` is the complete export of the owner's Claude design-system
artifact titled "ilex": Breeze frames, Adwaita interiors and holly green,
with light and dark tokens. Change values in that artifact and replace the
whole export; never edit it by hand. `.gitattributes` preserves its exact bytes.

The ilex shell will absorb this material and own its tokens, colors and other
design choices. Its components will read those tokens through generic code,
so a future design can be a token change inside the shell.
