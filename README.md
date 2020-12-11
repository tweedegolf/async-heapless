This library provides async synchronization primitives that do not depend on std
or alloc, currently a mutex and a oneshot channel.

The oneshot primitive is described in detail in an inline blog post, also
rendered at the [Tweede golf blog](https://tweedegolf.nl/blog/50/async-oneshot).
You can render it to markdown yourself with:

    sed -E 's/^/    /;s, +// ,,;s/ +$//' src/oneshot.rs
