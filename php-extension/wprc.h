/*
 * C ABI of the Rust library (php-extension/ffi). The Rust side never calls
 * into the Zend engine and the C side never holds a Rust lock: see
 * docs/ARCHITECTURE.md, section 3.
 */
#ifndef WPRC_H
#define WPRC_H

#include <stddef.h>
#include <stdint.h>

#define WPRC_TAG_NULL       0
#define WPRC_TAG_FALSE      1
#define WPRC_TAG_TRUE       2
#define WPRC_TAG_LONG       3
#define WPRC_TAG_DOUBLE     4
#define WPRC_TAG_STRING     5
#define WPRC_TAG_SERIALIZED 6

#define WPRC_MODE_SET     0
#define WPRC_MODE_ADD     1
#define WPRC_MODE_REPLACE 2

/* wprc_set() results */
#define WPRC_STORED    0
#define WPRC_EXISTS    1
#define WPRC_MISSING   2
#define WPRC_TOO_LARGE 3
#define WPRC_NO_MEMORY 4
#define WPRC_REJECTED  5
#define WPRC_ERROR    -1

/* A value leased from the Rust side: valid until wprc_value_release(). */
typedef struct {
	const char *ptr;
	size_t len;
	uint8_t tag;
	void *owner;
} wprc_value;

typedef struct {
	uint64_t hits, misses, sets, deletes, evictions, expired, stale, rejected;
	uint64_t too_large, no_memory, resets, contended, recoveries;
	uint64_t entries, payload_bytes, alloc_bytes, heap_bytes, total_size, max_item_size;
	uint64_t created_at, attaches, groups_used, namespaces, group_slots, groups_overflow, shards;
	uint64_t get_p50, get_p95, get_p99, set_p50, set_p95, set_p99, get_samples, set_samples;
	char policy[16];
	char path[256];
} wprc_stats;

/* Per request: records where the configuration lives (first call wins) and
 * re-attaches if the segment was retired or removed. Never creates it. */
void wprc_request_start(const char *config, size_t config_len, const char *segment, size_t segment_len);
/* Attaches if needed. 1 = usable. */
int wprc_ready(void);
/* Last attach error (empty when none). Valid until the next call. */
const char *wprc_error(size_t *len);

int64_t wprc_group(const char *ns, size_t ns_len, const char *group, size_t group_len);

/* 1 hit, 0 miss, -1 error */
int wprc_get(uint64_t gid, uint32_t blog, const char *key, size_t key_len, wprc_value *out);
void wprc_value_release(wprc_value *v);
int wprc_set(uint64_t gid, uint32_t blog, const char *key, size_t key_len,
	uint8_t tag, const char *val, size_t val_len, uint32_t ttl, int mode);
/* 1 deleted, 0 absent, -1 error */
int wprc_delete(uint64_t gid, uint32_t blog, const char *key, size_t key_len);
/* 1 ok (tag says which of lval/dval), 0 absent, -1 error */
int wprc_incr(uint64_t gid, uint32_t blog, const char *key, size_t key_len, int64_t offset,
	uint8_t *tag, int64_t *lval, double *dval);
int wprc_flush_group(uint64_t gid);
int wprc_flush_namespace(const char *ns, size_t ns_len);
int wprc_flush_all(void);
int wprc_read_stats(wprc_stats *out);
/* Shard resets this process performed since the last call, as one log line
 * (0 = none). */
size_t wprc_recovery_notice(char *buf, size_t cap);

#endif
