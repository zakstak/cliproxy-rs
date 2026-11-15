# Translation regression inputs

`translation_inputs/`, `chat_passthrough_inputs.json` and `patch_inputs.json`
retain historical request and peer-message stimuli. The pair files name their
original Go reference commit; patch sequences came from the v8.0.35 recorder.
Recorded output, output variants, decoder prose and normalization overlays were
removed in #198. These inputs are not provider-acceptance evidence.

Rust request/response pair, tool-search, patch and streaming-state contracts specify
expected fields independently. Corpus tests require success for schema-valid requests
unless `translation_refusals.json` or explicit input-type rules identify a refusal.
Native passthrough tests retain exact client bytes; generated JSON is checked by value.
The former Go recorders are available in Git history and are no longer a development
or validation toolchain.
