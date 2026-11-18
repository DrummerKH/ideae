# Themes

`ayu.json`, `catppuccin.json`, `everforest.json`, `flexoki.json`, `gruvbox.json`
and `solarized.json` are the theme files of
[gpui-component](https://github.com/longbridge/gpui-component) (Apache-2.0, `LICENSE-APACHE` here),
unchanged. Each names its own author in its `author` field.

The others are made here from the colors of the originals, the window's and the code's:

- `github.json`: GitHub Light and Dark of [github-vscode-theme](https://github.com/primer/github-vscode-theme) (MIT, `LICENSE-MIT`)
- `one.json`: One Light and One Dark of [Atom](https://github.com/atom/atom) (MIT, `LICENSE-MIT`)
- `rose-pine.json`: [Rosé Pine](https://github.com/rose-pine) and its Dawn (MIT, `LICENSE-MIT`)
- `tokyonight.json`: Night and Day of [tokyonight.nvim](https://github.com/folke/tokyonight.nvim) (Apache-2.0)
- `islands.json`: Islands Light and Dark of [IntelliJ](https://github.com/JetBrains/intellij-community/tree/master/platform/platform-resources/src/themes/islands) (Apache-2.0), with its Light and Islands Dark editor schemes

A theme is offered in the app when it is listed in `FAMILIES` in
`src/theme.rs`, with the names of its light and its dark side.
