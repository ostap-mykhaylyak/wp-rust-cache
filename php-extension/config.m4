dnl wp-rust-cache: thin C shim over a Rust static library.
dnl Build the library first:  cargo build --release -p wprc-ffi
dnl then:                     phpize && ./configure --enable-wp-rust-cache --with-wprc-lib=DIR

PHP_ARG_ENABLE([wp-rust-cache],
  [whether to enable wp-rust-cache],
  [AS_HELP_STRING([--enable-wp-rust-cache], [Enable the wp-rust-cache object cache])],
  [no])

PHP_ARG_WITH([wprc-lib],
  [directory containing libwprc_ffi.a],
  [AS_HELP_STRING([--with-wprc-lib=DIR], [Directory containing libwprc_ffi.a])],
  [no],
  [no])

if test "$PHP_WP_RUST_CACHE" != "no"; then
  if test "$PHP_WPRC_LIB" = "no" || test -z "$PHP_WPRC_LIB"; then
    AC_MSG_ERROR([--with-wprc-lib=DIR is required (the directory of libwprc_ffi.a)])
  fi
  if test ! -f "$PHP_WPRC_LIB/libwprc_ffi.a"; then
    AC_MSG_ERROR([libwprc_ffi.a not found in $PHP_WPRC_LIB; run: cargo build --release -p wprc-ffi])
  fi
  dnl --exclude-libs keeps the Rust runtime's symbols private to this module,
  dnl so it cannot clash with another extension that also embeds Rust.
  WP_RUST_CACHE_SHARED_LIBADD="$PHP_WPRC_LIB/libwprc_ffi.a -Wl,--exclude-libs,ALL -lpthread -ldl -lm -lrt"
  PHP_SUBST(WP_RUST_CACHE_SHARED_LIBADD)
  PHP_NEW_EXTENSION(wp_rust_cache, wp_rust_cache.c, $ext_shared,, -DZEND_ENABLE_STATIC_TSRMLS_CACHE=1)
fi
