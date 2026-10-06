/*
 * prism's allocator, where `.cargo/config.toml` builds prism with `PRISM_XALLOCATOR`: mimalloc,
 * the allocator the server runs on (`Cargo.toml`, `src/main.rs`).
 *
 * prism allocates and frees every node of a parse one by one, and the system allocator made that a
 * tenth of a walk of a text (`cursor::shapes`). prism frees only through `xfree`, and nothing
 * outside it frees what it allocated, so every pointer stays with the allocator that made it.
 * `src/lib.rs` links mimalloc into every binary, so these symbols are always there.
 */
#ifndef PRISM_XALLOCATOR_H
#define PRISM_XALLOCATOR_H

#include <stddef.h>

void *mi_malloc(size_t size);
void *mi_calloc(size_t count, size_t size);
void *mi_realloc(void *pointer, size_t size);
void mi_free(void *pointer);

#define xmalloc mi_malloc
#define xcalloc mi_calloc
#define xrealloc mi_realloc
#define xfree mi_free

#endif
