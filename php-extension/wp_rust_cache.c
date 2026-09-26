/*
 * wp-rust-cache PHP extension: zval codec and argument parsing.
 *
 * All cache logic lives in the Rust library (wprc.h). This file converts
 * PHP values to (tag, bytes) and back, and never holds a Rust lock: a PHP
 * fatal error raised here (memory_limit, a throwing __wakeup) can therefore
 * never leave shared memory locked.
 */
#ifdef HAVE_CONFIG_H
#include "config.h"
#endif

#include "php.h"
#include "php_ini.h"
#include "ext/standard/info.h"
#include "ext/standard/php_var.h"
#include "zend_exceptions.h"
#include "zend_smart_str.h"
#include "php_wp_rust_cache.h"
#include "wprc.h"

#include <string.h>

PHP_INI_BEGIN()
	PHP_INI_ENTRY("wp_rust_cache.enabled", "1", PHP_INI_SYSTEM, NULL)
	PHP_INI_ENTRY("wp_rust_cache.config", "/etc/wp-rust-cache/config.toml", PHP_INI_SYSTEM, NULL)
	/* Per-pool override of [shared_memory] path (php_admin_value). */
	PHP_INI_ENTRY("wp_rust_cache.segment", "", PHP_INI_SYSTEM, NULL)
PHP_INI_END()

static zend_always_inline int wprc_enabled(void)
{
	return INI_BOOL("wp_rust_cache.enabled");
}

/* ---- keys ------------------------------------------------------------- */

typedef struct {
	const char *ptr;
	size_t len;
	char buf[MAX_LENGTH_OF_LONG + 1];
} wprc_key;

/* WordPress keys are ints or strings; 5 and "5" are the same key. */
static zend_always_inline int wprc_key_from(zval *z, wprc_key *k)
{
	ZVAL_DEREF(z);
	if (Z_TYPE_P(z) == IS_STRING) {
		k->ptr = Z_STRVAL_P(z);
		k->len = Z_STRLEN_P(z);
		return 1;
	}
	if (Z_TYPE_P(z) == IS_LONG) {
		k->len = (size_t) snprintf(k->buf, sizeof(k->buf), ZEND_LONG_FMT, Z_LVAL_P(z));
		k->ptr = k->buf;
		return 1;
	}
	return 0;
}

static zend_always_inline void wprc_key_from_hash(zend_ulong idx, zend_string *s, wprc_key *k)
{
	if (s) {
		k->ptr = ZSTR_VAL(s);
		k->len = ZSTR_LEN(s);
	} else {
		k->len = (size_t) snprintf(k->buf, sizeof(k->buf), ZEND_ULONG_FMT, idx);
		k->ptr = k->buf;
	}
}

/* ---- values ----------------------------------------------------------- */

typedef struct {
	uint8_t tag;
	const char *ptr;
	size_t len;
	char num[8];
	smart_str ser;
} wprc_enc;

/* Scalars are stored natively (incr/decr work on them in shared memory);
 * arrays and objects go through PHP's own serializer, the only format that
 * preserves __serialize/__unserialize, __sleep/__wakeup and class identity. */
static int wprc_encode(zval *v, wprc_enc *e)
{
	memset(&e->ser, 0, sizeof(e->ser));
	e->ptr = e->num;
	e->len = 0;
	ZVAL_DEREF(v);
	switch (Z_TYPE_P(v)) {
	case IS_UNDEF:
	case IS_NULL:
		e->tag = WPRC_TAG_NULL;
		return SUCCESS;
	case IS_FALSE:
		e->tag = WPRC_TAG_FALSE;
		return SUCCESS;
	case IS_TRUE:
		e->tag = WPRC_TAG_TRUE;
		return SUCCESS;
	case IS_LONG: {
		int64_t l = (int64_t) Z_LVAL_P(v);
		memcpy(e->num, &l, 8);
		e->tag = WPRC_TAG_LONG;
		e->len = 8;
		return SUCCESS;
	}
	case IS_DOUBLE: {
		double d = Z_DVAL_P(v);
		memcpy(e->num, &d, 8);
		e->tag = WPRC_TAG_DOUBLE;
		e->len = 8;
		return SUCCESS;
	}
	case IS_STRING:
		e->tag = WPRC_TAG_STRING;
		e->ptr = Z_STRVAL_P(v);
		e->len = Z_STRLEN_P(v);
		return SUCCESS;
	default: {
		php_serialize_data_t vh;
		PHP_VAR_SERIALIZE_INIT(vh);
		php_var_serialize(&e->ser, v, &vh);
		PHP_VAR_SERIALIZE_DESTROY(vh);
		if (EG(exception)) {
			/* e.g. a Closure. Core's cache would keep it in memory only,
			 * and so will the drop-in; the exception is not ours to raise. */
			zend_clear_exception();
			smart_str_free(&e->ser);
			return FAILURE;
		}
		if (!e->ser.s) {
			return FAILURE;
		}
		e->tag = WPRC_TAG_SERIALIZED;
		e->ptr = ZSTR_VAL(e->ser.s);
		e->len = ZSTR_LEN(e->ser.s);
		return SUCCESS;
	}
	}
}

static zend_always_inline void wprc_enc_free(wprc_enc *e)
{
	smart_str_free(&e->ser);
}

static int wprc_decode(uint8_t tag, const char *p, size_t len, zval *rv)
{
	switch (tag) {
	case WPRC_TAG_NULL:
		ZVAL_NULL(rv);
		return SUCCESS;
	case WPRC_TAG_FALSE:
		ZVAL_FALSE(rv);
		return SUCCESS;
	case WPRC_TAG_TRUE:
		ZVAL_TRUE(rv);
		return SUCCESS;
	case WPRC_TAG_LONG: {
		int64_t l;
		if (len != 8) return FAILURE;
		memcpy(&l, p, 8);
		ZVAL_LONG(rv, (zend_long) l);
		return SUCCESS;
	}
	case WPRC_TAG_DOUBLE: {
		double d;
		if (len != 8) return FAILURE;
		memcpy(&d, p, 8);
		ZVAL_DOUBLE(rv, d);
		return SUCCESS;
	}
	case WPRC_TAG_STRING:
		ZVAL_STRINGL(rv, p, len);
		return SUCCESS;
	case WPRC_TAG_SERIALIZED: {
		php_unserialize_data_t vh;
		const unsigned char *cur = (const unsigned char *) p;
		int ok;
		PHP_VAR_UNSERIALIZE_INIT(vh);
		ok = php_var_unserialize(rv, &cur, cur + len, &vh);
		PHP_VAR_UNSERIALIZE_DESTROY(vh);
		if (!ok) {
			zval_ptr_dtor(rv);
			ZVAL_UNDEF(rv);
			return FAILURE;
		}
		return SUCCESS;
	}
	}
	return FAILURE;
}

/* Fetches and decodes one key into rv. 1 hit, 0 miss. */
static int wprc_fetch(uint64_t gid, uint32_t blog, const wprc_key *k, zval *rv)
{
	wprc_value v;
	int r = wprc_get(gid, blog, k->ptr, k->len, &v);
	if (r != 1) {
		return 0;
	}
	/* The leased buffer stays ours during decoding even if an autoloader
	 * triggered by unserialize() re-enters the cache. */
	r = wprc_decode(v.tag, v.ptr, v.len, rv);
	wprc_value_release(&v);
	if (r == FAILURE) {
		/* Undecodable (class gone, corrupt payload): drop it so the next
		 * request rebuilds it instead of failing again. */
		if (!EG(exception)) {
			wprc_delete(gid, blog, k->ptr, k->len);
		}
		return 0;
	}
	return 1;
}

static int wprc_store(uint64_t gid, uint32_t blog, const wprc_key *k, zval *val, zend_long ttl, int mode)
{
	wprc_enc e;
	uint32_t t;
	int r;
	if (wprc_encode(val, &e) == FAILURE) {
		/* Never let an older value survive a write that did not happen. */
		if (mode == WPRC_MODE_SET) {
			wprc_delete(gid, blog, k->ptr, k->len);
		}
		return 0;
	}
	t = ttl <= 0 ? 0 : (ttl > (zend_long) UINT32_MAX ? UINT32_MAX : (uint32_t) ttl);
	r = wprc_set(gid, blog, k->ptr, k->len, e.tag, e.ptr, e.len, t, mode);
	wprc_enc_free(&e);
	return r == WPRC_STORED || r == WPRC_REJECTED;
}

/* ---- PHP functions ---------------------------------------------------- */

#define WPRC_GID_BLOG(gid, blog) (uint64_t) (gid), (uint32_t) (blog)

/* The site keeps working without the segment (the drop-in falls back to a
 * per-request cache), so a broken setup would go unnoticed: say why in the
 * PHP error log, once per distinct reason per process. */
static void wprc_log_unavailable(void)
{
	ZEND_TLS char last[512]; /* ZEND_TLS includes "static" */
	size_t len = 0;
	const char *err = wprc_error(&len);
	char line[640];
	if (!err || !len) {
		return;
	}
	if (len >= sizeof(last)) {
		len = sizeof(last) - 1;
	}
	if (strlen(last) == len && memcmp(last, err, len) == 0) {
		return;
	}
	memcpy(last, err, len);
	last[len] = '\0';
	snprintf(line, sizeof(line), "wp-rust-cache: shared memory unavailable, running without a persistent cache: %s", last);
	php_log_err(line);
}

static void wprc_start(void)
{
	const char *cfg = INI_STR("wp_rust_cache.config");
	const char *seg = INI_STR("wp_rust_cache.segment");
	wprc_request_start(cfg, cfg ? strlen(cfg) : 0, seg, seg ? strlen(seg) : 0);
}

PHP_FUNCTION(wp_rust_cache_available)
{
	ZEND_PARSE_PARAMETERS_NONE();
	if (!wprc_enabled()) {
		RETURN_FALSE;
	}
	/* The drop-in calls this once per WordPress request. Repeating the
	 * request-start check here covers SAPIs that do not run RINIT per
	 * request (FrankenPHP worker mode). */
	wprc_start();
	if (wprc_ready() == 1) {
		RETURN_TRUE;
	}
	wprc_log_unavailable();
	RETURN_FALSE;
}

PHP_FUNCTION(wp_rust_cache_group)
{
	zend_string *ns, *group;
	int64_t id;
	ZEND_PARSE_PARAMETERS_START(2, 2)
		Z_PARAM_STR(ns)
		Z_PARAM_STR(group)
	ZEND_PARSE_PARAMETERS_END();
	if (!wprc_enabled()) RETURN_FALSE;
	id = wprc_group(ZSTR_VAL(ns), ZSTR_LEN(ns), ZSTR_VAL(group), ZSTR_LEN(group));
	if (id < 0) RETURN_FALSE;
	RETURN_LONG((zend_long) id);
}

PHP_FUNCTION(wp_rust_cache_get)
{
	zend_long gid, blog;
	zval *zkey, *found = NULL;
	wprc_key k;
	ZEND_PARSE_PARAMETERS_START(3, 4)
		Z_PARAM_LONG(gid)
		Z_PARAM_LONG(blog)
		Z_PARAM_ZVAL(zkey)
		Z_PARAM_OPTIONAL
		Z_PARAM_ZVAL(found)
	ZEND_PARSE_PARAMETERS_END();
	if (wprc_key_from(zkey, &k) && wprc_fetch(WPRC_GID_BLOG(gid, blog), &k, return_value)) {
		if (found) ZEND_TRY_ASSIGN_REF_TRUE(found);
		return;
	}
	if (found) ZEND_TRY_ASSIGN_REF_FALSE(found);
	RETURN_FALSE;
}

/* Returns [key => value] for the keys that were found. */
PHP_FUNCTION(wp_rust_cache_get_multiple)
{
	zend_long gid, blog;
	HashTable *keys;
	zval *zkey;
	ZEND_PARSE_PARAMETERS_START(3, 3)
		Z_PARAM_LONG(gid)
		Z_PARAM_LONG(blog)
		Z_PARAM_ARRAY_HT(keys)
	ZEND_PARSE_PARAMETERS_END();
	array_init_size(return_value, zend_hash_num_elements(keys));
	ZEND_HASH_FOREACH_VAL(keys, zkey) {
		wprc_key k;
		zval v;
		if (!wprc_key_from(zkey, &k)) continue;
		if (wprc_fetch(WPRC_GID_BLOG(gid, blog), &k, &v)) {
			zend_symtable_str_update(Z_ARRVAL_P(return_value), k.ptr, k.len, &v);
		}
		if (EG(exception)) return;
	} ZEND_HASH_FOREACH_END();
}

static void wprc_set_impl(INTERNAL_FUNCTION_PARAMETERS, int mode)
{
	zend_long gid, blog, ttl = 0;
	zval *zkey, *val;
	wprc_key k;
	ZEND_PARSE_PARAMETERS_START(4, 5)
		Z_PARAM_LONG(gid)
		Z_PARAM_LONG(blog)
		Z_PARAM_ZVAL(zkey)
		Z_PARAM_ZVAL(val)
		Z_PARAM_OPTIONAL
		Z_PARAM_LONG(ttl)
	ZEND_PARSE_PARAMETERS_END();
	if (!wprc_key_from(zkey, &k)) RETURN_FALSE;
	RETURN_BOOL(wprc_store(WPRC_GID_BLOG(gid, blog), &k, val, ttl, mode));
}

PHP_FUNCTION(wp_rust_cache_set) { wprc_set_impl(INTERNAL_FUNCTION_PARAM_PASSTHRU, WPRC_MODE_SET); }
PHP_FUNCTION(wp_rust_cache_add) { wprc_set_impl(INTERNAL_FUNCTION_PARAM_PASSTHRU, WPRC_MODE_ADD); }
PHP_FUNCTION(wp_rust_cache_replace) { wprc_set_impl(INTERNAL_FUNCTION_PARAM_PASSTHRU, WPRC_MODE_REPLACE); }

/* [key => value] in, [key => bool] out. */
PHP_FUNCTION(wp_rust_cache_set_multiple)
{
	zend_long gid, blog, ttl = 0;
	HashTable *items;
	zend_ulong idx;
	zend_string *skey;
	zval *val;
	ZEND_PARSE_PARAMETERS_START(3, 4)
		Z_PARAM_LONG(gid)
		Z_PARAM_LONG(blog)
		Z_PARAM_ARRAY_HT(items)
		Z_PARAM_OPTIONAL
		Z_PARAM_LONG(ttl)
	ZEND_PARSE_PARAMETERS_END();
	array_init_size(return_value, zend_hash_num_elements(items));
	ZEND_HASH_FOREACH_KEY_VAL(items, idx, skey, val) {
		wprc_key k;
		wprc_key_from_hash(idx, skey, &k);
		if (skey) {
			add_assoc_bool_ex(return_value, k.ptr, k.len, wprc_store(WPRC_GID_BLOG(gid, blog), &k, val, ttl, WPRC_MODE_SET));
		} else {
			add_index_bool(return_value, idx, wprc_store(WPRC_GID_BLOG(gid, blog), &k, val, ttl, WPRC_MODE_SET));
		}
	} ZEND_HASH_FOREACH_END();
}

PHP_FUNCTION(wp_rust_cache_delete)
{
	zend_long gid, blog;
	zval *zkey;
	wprc_key k;
	ZEND_PARSE_PARAMETERS_START(3, 3)
		Z_PARAM_LONG(gid)
		Z_PARAM_LONG(blog)
		Z_PARAM_ZVAL(zkey)
	ZEND_PARSE_PARAMETERS_END();
	if (!wprc_key_from(zkey, &k)) RETURN_FALSE;
	RETURN_BOOL(wprc_delete(WPRC_GID_BLOG(gid, blog), k.ptr, k.len) == 1);
}

/* [key, ...] in, [key => bool] out. */
PHP_FUNCTION(wp_rust_cache_delete_multiple)
{
	zend_long gid, blog;
	HashTable *keys;
	zval *zkey;
	ZEND_PARSE_PARAMETERS_START(3, 3)
		Z_PARAM_LONG(gid)
		Z_PARAM_LONG(blog)
		Z_PARAM_ARRAY_HT(keys)
	ZEND_PARSE_PARAMETERS_END();
	array_init_size(return_value, zend_hash_num_elements(keys));
	ZEND_HASH_FOREACH_VAL(keys, zkey) {
		wprc_key k;
		zval b;
		if (!wprc_key_from(zkey, &k)) continue;
		ZVAL_BOOL(&b, wprc_delete(WPRC_GID_BLOG(gid, blog), k.ptr, k.len) == 1);
		zend_symtable_str_update(Z_ARRVAL_P(return_value), k.ptr, k.len, &b);
	} ZEND_HASH_FOREACH_END();
}

/* Atomic in shared memory; a negative offset decrements. int|float|false. */
PHP_FUNCTION(wp_rust_cache_incr)
{
	zend_long gid, blog, offset = 1;
	zval *zkey;
	wprc_key k;
	uint8_t tag = 0;
	int64_t l = 0;
	double d = 0;
	ZEND_PARSE_PARAMETERS_START(3, 4)
		Z_PARAM_LONG(gid)
		Z_PARAM_LONG(blog)
		Z_PARAM_ZVAL(zkey)
		Z_PARAM_OPTIONAL
		Z_PARAM_LONG(offset)
	ZEND_PARSE_PARAMETERS_END();
	if (!wprc_key_from(zkey, &k)) RETURN_FALSE;
	if (wprc_incr(WPRC_GID_BLOG(gid, blog), k.ptr, k.len, (int64_t) offset, &tag, &l, &d) != 1) {
		RETURN_FALSE;
	}
	if (tag == WPRC_TAG_DOUBLE) RETURN_DOUBLE(d);
	RETURN_LONG((zend_long) l);
}

PHP_FUNCTION(wp_rust_cache_flush_group)
{
	zend_long gid;
	ZEND_PARSE_PARAMETERS_START(1, 1)
		Z_PARAM_LONG(gid)
	ZEND_PARSE_PARAMETERS_END();
	RETURN_BOOL(wprc_flush_group((uint64_t) gid) == 1);
}

PHP_FUNCTION(wp_rust_cache_flush_namespace)
{
	zend_string *ns;
	ZEND_PARSE_PARAMETERS_START(1, 1)
		Z_PARAM_STR(ns)
	ZEND_PARSE_PARAMETERS_END();
	if (!wprc_enabled()) RETURN_FALSE;
	RETURN_BOOL(wprc_flush_namespace(ZSTR_VAL(ns), ZSTR_LEN(ns)) == 1);
}

PHP_FUNCTION(wp_rust_cache_flush_all)
{
	ZEND_PARSE_PARAMETERS_NONE();
	if (!wprc_enabled()) RETURN_FALSE;
	RETURN_BOOL(wprc_flush_all() == 1);
}

#define WPRC_STAT(name) add_assoc_long(return_value, #name, (zend_long) s.name)

PHP_FUNCTION(wp_rust_cache_stats)
{
	wprc_stats s;
	ZEND_PARSE_PARAMETERS_NONE();
	if (!wprc_enabled() || wprc_read_stats(&s) != 1) RETURN_FALSE;
	array_init(return_value);
	add_assoc_string(return_value, "path", s.path);
	add_assoc_string(return_value, "policy", s.policy);
	WPRC_STAT(shards);
	WPRC_STAT(total_size);
	WPRC_STAT(heap_bytes);
	WPRC_STAT(max_item_size);
	WPRC_STAT(created_at);
	WPRC_STAT(attaches);
	WPRC_STAT(hits);
	WPRC_STAT(misses);
	WPRC_STAT(sets);
	WPRC_STAT(deletes);
	WPRC_STAT(evictions);
	WPRC_STAT(expired);
	WPRC_STAT(stale);
	WPRC_STAT(rejected);
	WPRC_STAT(too_large);
	WPRC_STAT(no_memory);
	WPRC_STAT(resets);
	WPRC_STAT(recoveries);
	WPRC_STAT(contended);
	WPRC_STAT(entries);
	WPRC_STAT(payload_bytes);
	WPRC_STAT(alloc_bytes);
	WPRC_STAT(namespaces);
	WPRC_STAT(groups_used);
	WPRC_STAT(group_slots);
	WPRC_STAT(groups_overflow);
	WPRC_STAT(get_p50);
	WPRC_STAT(get_p95);
	WPRC_STAT(get_p99);
	WPRC_STAT(set_p50);
	WPRC_STAT(set_p95);
	WPRC_STAT(set_p99);
	WPRC_STAT(get_samples);
	WPRC_STAT(set_samples);
	add_assoc_double(return_value, "hit_ratio",
		s.hits + s.misses ? (double) s.hits / (double) (s.hits + s.misses) : 0.0);
}

PHP_FUNCTION(wp_rust_cache_info)
{
	size_t elen = 0;
	const char *err;
	int ready;
	ZEND_PARSE_PARAMETERS_NONE();
	ready = wprc_enabled() && wprc_ready() == 1;
	err = wprc_error(&elen);
	array_init(return_value);
	add_assoc_string(return_value, "version", PHP_WP_RUST_CACHE_VERSION);
	add_assoc_bool(return_value, "enabled", wprc_enabled());
	add_assoc_bool(return_value, "attached", ready);
	add_assoc_stringl(return_value, "error", err ? err : "", err ? elen : 0);
	add_assoc_string(return_value, "config", INI_STR("wp_rust_cache.config"));
}

/* ---- arginfo ---------------------------------------------------------- */

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_available, 0, 0, _IS_BOOL, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_MASK_EX(arginfo_group, 0, 2, MAY_BE_LONG | MAY_BE_FALSE)
	ZEND_ARG_TYPE_INFO(0, namespace, IS_STRING, 0)
	ZEND_ARG_TYPE_INFO(0, group, IS_STRING, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_get, 0, 3, IS_MIXED, 0)
	ZEND_ARG_TYPE_INFO(0, group_id, IS_LONG, 0)
	ZEND_ARG_TYPE_INFO(0, blog_id, IS_LONG, 0)
	ZEND_ARG_TYPE_MASK(0, key, MAY_BE_LONG | MAY_BE_STRING, NULL)
	ZEND_ARG_INFO_WITH_DEFAULT_VALUE(1, found, "null")
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_keys_array, 0, 3, IS_ARRAY, 0)
	ZEND_ARG_TYPE_INFO(0, group_id, IS_LONG, 0)
	ZEND_ARG_TYPE_INFO(0, blog_id, IS_LONG, 0)
	ZEND_ARG_TYPE_INFO(0, keys, IS_ARRAY, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_set, 0, 4, _IS_BOOL, 0)
	ZEND_ARG_TYPE_INFO(0, group_id, IS_LONG, 0)
	ZEND_ARG_TYPE_INFO(0, blog_id, IS_LONG, 0)
	ZEND_ARG_TYPE_MASK(0, key, MAY_BE_LONG | MAY_BE_STRING, NULL)
	ZEND_ARG_TYPE_INFO(0, value, IS_MIXED, 0)
	ZEND_ARG_TYPE_INFO_WITH_DEFAULT_VALUE(0, ttl, IS_LONG, 0, "0")
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_set_multiple, 0, 3, IS_ARRAY, 0)
	ZEND_ARG_TYPE_INFO(0, group_id, IS_LONG, 0)
	ZEND_ARG_TYPE_INFO(0, blog_id, IS_LONG, 0)
	ZEND_ARG_TYPE_INFO(0, items, IS_ARRAY, 0)
	ZEND_ARG_TYPE_INFO_WITH_DEFAULT_VALUE(0, ttl, IS_LONG, 0, "0")
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_delete, 0, 3, _IS_BOOL, 0)
	ZEND_ARG_TYPE_INFO(0, group_id, IS_LONG, 0)
	ZEND_ARG_TYPE_INFO(0, blog_id, IS_LONG, 0)
	ZEND_ARG_TYPE_MASK(0, key, MAY_BE_LONG | MAY_BE_STRING, NULL)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_MASK_EX(arginfo_incr, 0, 3, MAY_BE_LONG | MAY_BE_DOUBLE | MAY_BE_FALSE)
	ZEND_ARG_TYPE_INFO(0, group_id, IS_LONG, 0)
	ZEND_ARG_TYPE_INFO(0, blog_id, IS_LONG, 0)
	ZEND_ARG_TYPE_MASK(0, key, MAY_BE_LONG | MAY_BE_STRING, NULL)
	ZEND_ARG_TYPE_INFO_WITH_DEFAULT_VALUE(0, offset, IS_LONG, 0, "1")
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_flush_group, 0, 1, _IS_BOOL, 0)
	ZEND_ARG_TYPE_INFO(0, group_id, IS_LONG, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_flush_namespace, 0, 1, _IS_BOOL, 0)
	ZEND_ARG_TYPE_INFO(0, namespace, IS_STRING, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_MASK_EX(arginfo_stats, 0, 0, MAY_BE_ARRAY | MAY_BE_FALSE)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_info, 0, 0, IS_ARRAY, 0)
ZEND_END_ARG_INFO()

static const zend_function_entry wp_rust_cache_functions[] = {
	ZEND_FE(wp_rust_cache_available, arginfo_available)
	ZEND_FE(wp_rust_cache_group, arginfo_group)
	ZEND_FE(wp_rust_cache_get, arginfo_get)
	ZEND_FE(wp_rust_cache_get_multiple, arginfo_keys_array)
	ZEND_FE(wp_rust_cache_set, arginfo_set)
	ZEND_FE(wp_rust_cache_add, arginfo_set)
	ZEND_FE(wp_rust_cache_replace, arginfo_set)
	ZEND_FE(wp_rust_cache_set_multiple, arginfo_set_multiple)
	ZEND_FE(wp_rust_cache_delete, arginfo_delete)
	ZEND_FE(wp_rust_cache_delete_multiple, arginfo_keys_array)
	ZEND_FE(wp_rust_cache_incr, arginfo_incr)
	ZEND_FE(wp_rust_cache_flush_group, arginfo_flush_group)
	ZEND_FE(wp_rust_cache_flush_namespace, arginfo_flush_namespace)
	ZEND_FE(wp_rust_cache_flush_all, arginfo_available)
	ZEND_FE(wp_rust_cache_stats, arginfo_stats)
	ZEND_FE(wp_rust_cache_info, arginfo_info)
	ZEND_FE_END
};

/* ---- module ----------------------------------------------------------- */

PHP_MINIT_FUNCTION(wp_rust_cache)
{
#if defined(ZTS) && defined(COMPILE_DL_WP_RUST_CACHE)
	ZEND_TSRMLS_CACHE_UPDATE();
#endif
	REGISTER_INI_ENTRIES();
	/* Deliberately no attach here: in PHP-FPM this runs in the master, as
	 * root, and a mapping made now would be inherited by every pool. */
	return SUCCESS;
}

PHP_MSHUTDOWN_FUNCTION(wp_rust_cache)
{
	UNREGISTER_INI_ENTRIES();
	return SUCCESS;
}

PHP_RINIT_FUNCTION(wp_rust_cache)
{
#if defined(ZTS) && defined(COMPILE_DL_WP_RUST_CACHE)
	ZEND_TSRMLS_CACHE_UPDATE();
#endif
	if (wprc_enabled()) {
		/* Cheap: records the settings once, then one atomic load and one
		 * fstat to notice a retired or removed segment. */
		wprc_start();
	}
	return SUCCESS;
}

PHP_MINFO_FUNCTION(wp_rust_cache)
{
	size_t elen = 0;
	const char *err;
	php_info_print_table_start();
	php_info_print_table_row(2, "wp-rust-cache", "enabled");
	php_info_print_table_row(2, "Version", PHP_WP_RUST_CACHE_VERSION);
	php_info_print_table_row(2, "Backend", "shared memory (Rust engine)");
	err = wprc_error(&elen);
	php_info_print_table_row(2, "Last attach error", (err && elen) ? err : "none");
	php_info_print_table_end();
	DISPLAY_INI_ENTRIES();
}

zend_module_entry wp_rust_cache_module_entry = {
	STANDARD_MODULE_HEADER,
	"wp_rust_cache",
	wp_rust_cache_functions,
	PHP_MINIT(wp_rust_cache),
	PHP_MSHUTDOWN(wp_rust_cache),
	PHP_RINIT(wp_rust_cache),
	NULL,
	PHP_MINFO(wp_rust_cache),
	PHP_WP_RUST_CACHE_VERSION,
	STANDARD_MODULE_PROPERTIES
};

#ifdef COMPILE_DL_WP_RUST_CACHE
#ifdef ZTS
ZEND_TSRMLS_CACHE_DEFINE()
#endif
ZEND_GET_MODULE(wp_rust_cache)
#endif
