# pyinfra_do_smoke

Reference experiment for the `~/.kresko/` layout. To install it::

    cp -r experiments/pyinfra_do_smoke ~/.kresko/experiments/

Then drive it with the `kresko` CLI::

    kresko run pyinfra_do_smoke -- plan
    kresko run pyinfra_do_smoke --name regression-1 -- up
    kresko run pyinfra_do_smoke --name regression-1 -- deploy
    kresko run pyinfra_do_smoke --name regression-1 -- smoke
    kresko run pyinfra_do_smoke --name regression-1 -- collect
    kresko run pyinfra_do_smoke --name regression-1 -- down

Each invocation creates a new run dir under
`~/.kresko/runs/pyinfra_do_smoke/<run-name>/` (or `<run-name>-2`, `-3`, … on
collision). All outputs — manifest, results, pyinfra inventory/deploy files,
node snapshots, collected data, logs — live inside that directory.

The `rust-help` action demonstrates calling the Rust `kresko` binary via
`experiment.shell()` for things like `genesis` or `txblast-local`.
