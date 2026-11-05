def "nu-complete mnn targets" [] {
  [all apk apt aur auto brew bun choco debian determinate dnf emerge eopkg flatpak pacman pip pkg pnpm rpm-ostree snap soar transactional-update uv winget xbps yum zypper]
}

def "nu-complete mnn completion-modes" [] {
  [bash fish zsh nu all detect]
}

export extern "mnn" [
  --dry-run(-n)
  --silent(-s)
  --verbose(-v)
  --disable-log
  --parallel
  --no-parallel(-P)
  --gen-config(-c)
  --install-completions: string@"nu-complete mnn completion-modes"
  --print-completion: string@"nu-complete mnn completion-modes"
  --install(-i)
  --reinstall(-r)
  --update(-u)
  --uninstall(-x)
  --purge(-p)
  --help(-h)
  ...targets: string@"nu-complete mnn targets"
]
