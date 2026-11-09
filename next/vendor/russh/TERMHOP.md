# russh 0.65.1 (vendored)

Copy of [russh](https://github.com/warp-tech/russh) 0.65.1 (Apache-2.0, see LICENSE.txt) with one change,
marked `termhop patch` in `src/client/mod.rs`: the client may request Diffie-Hellman group exchange
moduli down to 1024 bits, the same lower bound PuTTY uses, instead of 2048.
Examples, tests and benches are left out.
