# git-suggest-ownership

CLI tool that lists the files in a git repository that currently have lines that were changed by you.
Sorted by percentage of lines you changed for each file.

Useful to figure out what parts of the code you could be _codeowner_ for.

It does this by blaming each file in the repo. Which is quite slow unfortunately.

# Usage
```bash
cd pathtoyourrepo
git-suggest-ownership
```

For this repo the output would be something like this, since there is only one contributor:
```
/ - 100.0%
├── .github - 100.0%
│   └── workflows - 100.0%
│       ├── release.yml - 100.0%
│       └── test.yml - 100.0%
├── .gitignore - 100.0%
├── Cargo.lock - 100.0%
├── Cargo.toml - 100.0%
├── LICENSE - 100.0%
├── README.md - 100.0%
└── src - 100.0%
    └── main.rs - 100.0%
```