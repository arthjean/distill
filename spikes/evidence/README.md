# EP-002 evidence

`spikes/shared/evaluate.mjs` writes candidate conformance, release benchmark,
and CPU-accounted fuzz evidence here. JSON evidence includes the machine,
binary path, protocol and corpus digests, raw samples, and pass/fail gates.

Evidence is committed only after the corresponding command completes. Temporary
stores use `.tmp-*` directories and are ignored.
