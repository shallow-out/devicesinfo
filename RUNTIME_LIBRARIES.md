# Runtime library observations

Environment reports now include an optional `inference_libraries` list for
`librkllmrt.so` and `librknnrt.so`. The target filesystem is checked in the declared
library paths, using the same path list for local probes and fixture/remote input
collection. The first observed file per library is reported once; directories
and broken links are not library files. Empty lists are omitted from JSON and
older reports deserialize with an empty default.

This indicates file presence only. It does not claim ABI compatibility, loading,
model conversion, or successful inference. Deployment remains envsetupd's job;
mesh capabilities remain explicitly declared by the target llmd.
