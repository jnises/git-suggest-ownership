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