# Local Calamine patches

This is Calamine 0.38.0, vendored under its MIT license for bounded legacy
`.xls` parsing in Find.

`src/xls.rs` caps cumulative dense worksheet and formula storage at 250,000
cells. XLS `Dimensions` records are untrusted, so their declared area is used
for preallocation only when it is within that limit. Actual sparse cell
coordinates are checked before `Range::from_sparse` creates dense storage.
Oversized workbooks return `XlsError::ResourceLimit` and Find treats them as
non-matches.

Retain these guards when updating Calamine. The upstream `Range::from_sparse`
constructor allocates the complete rectangle between the outermost cells.
