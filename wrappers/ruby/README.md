# trail (Ruby wrapper)

A thin Ruby wrapper that shells out to the `trail` binary and parses its JSON.
The CLI is the source of truth. Requires `trail` on `PATH` (or set `TRAIL_BIN`).

```ruby
require_relative "trail"

c = Trail::Client.new("/repo")
c.init
while (folder = c.claim("refine", agent: "a1")) # nil = sweep complete
  investigate(folder["path"])
  c.done("refine", folder["path"], agent: "a1", found: 3)
end
p c.status("refine")
```

`claim` returns `nil` when the sweep is complete and retries internally while
folders are only leased elsewhere (exit 4). `done`/`skip` accept `found:` (or
`clean: true`) for outcome weighting.
