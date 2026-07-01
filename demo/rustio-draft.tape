# Record the usage GIF shown in the README.
#
#   cargo install --path .              # put `rustio-draft` on PATH
#   export ANTHROPIC_API_KEY=sk-ant-... # the run makes a real Claude call
#   vhs demo/rustio-draft.tape          # → writes demo/rustio-draft.gif
#
# VHS: https://github.com/charmbracelet/vhs
Require rustio-draft

Output demo/rustio-draft.gif

Set Shell "bash"
Set FontSize 15
Set Width 1120
Set Height 680
Set Padding 18
Set Theme "Catppuccin Mocha"

Type "rustio-draft new 'a blog with posts and comments'"
Sleep 800ms
Enter
# generation makes a live API call — give it room (tune to your latency)
Sleep 18s

Type "cat schema.json"
Sleep 800ms
Enter
Sleep 5s
