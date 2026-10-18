# Brand icon

`shipments.svg` is the editable source for the parcel icon. Export transparent
PNG files at 256 and 512 pixels to `custom_components/shipments/brand/icon.png`
and `icon@2x.png`, and at 128 pixels to `shipments/icon.png` for the app.

For `dark_icon.png` and `dark_icon@2x.png`, use these palette substitutions:

| Light | Dark |
| --- | --- |
| `#1d1e20` | `#f5f5f3` |
| `#f0f0ee` | `#35383d` |
| `#ffffff` | `#25282c` |
| `#e1e1de` | `#44484e` |

On macOS, `sips -s format png source.svg --out icon.png` exports the SVG at its
declared width and height. Set those dimensions to the target size while
preserving the `viewBox`, then optimize the PNG losslessly.
