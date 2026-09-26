<?php
// Worker-mode endpoint: this script stays loaded and handles many requests,
// so RINIT does not run per request. wp_rust_cache_available() is what the
// drop-in calls at the start of each WordPress request; it performs the
// request-start check (retired or deleted segment) itself.
$handler = static function () {
	wp_rust_cache_available();
	$g  = wp_rust_cache_group( 'frankenphp', 'worker' );
	$op = $_GET['op'] ?? 'incr';
	if ( 'reset' === $op ) {
		echo wp_rust_cache_set( $g, 0, 'counter', 0 ) ? 'OK' : 'FAIL';
	} elseif ( 'get' === $op ) {
		$found = null;
		$v     = wp_rust_cache_get( $g, 0, 'counter', $found );
		echo $found ? $v : 'MISSING';
	} else {
		echo wp_rust_cache_incr( $g, 0, 'counter', 1 ) === false ? 'FAIL' : 'OK';
	}
};
while ( frankenphp_handle_request( $handler ) ) {
	gc_collect_cycles();
}
