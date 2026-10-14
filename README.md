# musik

Apple Music TUI player for macOS.

## Install

```bash
brew tap palmachris7/tap
brew install palmachris7/tap/musik
```

Apple Silicon uses the release binary. Other architectures build from source.

## Usage

```bash
musik
musik version
musik --cover foto
musik --cover halfblocks
musik --hidpi 2
```

Inside the player:

- `/` finder
- `Tab` switch Library / Apple Music
- `Enter` or `→` play/open
- `Esc` or `←` back/close
- `+` add to playlist
- `m` menu
- `c` toggle cover
- `C` switch cover mode
- `l` toggle lyrics

Preferences persist in `~/.config/musik/prefs.json`.
