#ifndef PHP_WP_RUST_CACHE_H
#define PHP_WP_RUST_CACHE_H

extern zend_module_entry wp_rust_cache_module_entry;
#define phpext_wp_rust_cache_ptr &wp_rust_cache_module_entry

#define PHP_WP_RUST_CACHE_VERSION "0.1.2"

#if defined(ZTS) && defined(COMPILE_DL_WP_RUST_CACHE)
ZEND_TSRMLS_CACHE_EXTERN()
#endif

#endif
