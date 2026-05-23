# Changesets

This folder holds [changesets](https://github.com/changesets/changesets) - small markdown files describing pending releases.

## Adding a changeset

```bash
yarn changeset
```

Pick the affected packages and bump kind (patch/minor/major), then commit the generated file alongside your change.

## How releases happen

Pushes to `main` run the `release` job in CI. It calls [`changesets/action`](https://github.com/changesets/action), which behaves in one of two ways:

### Pending changesets exist

The action opens (or updates) a "chore(release): version packages" PR that runs `yarn version-packages` (`changeset version`). Merging that PR bumps versions and clears the `.changeset/*.md` files.

### No pending changesets

The action runs `yarn release` (`changeset publish`), which publishes any workspace packages whose versions are ahead of npm. Native platform sub-packages are published via `@botejs/native`'s `prepublishOnly` (`napi prepublish -t npm --no-gh-release`).

`@botejs/native` and `bote` are linked, so they always bump in lockstep. `@botejs/bench` is ignored (private).
