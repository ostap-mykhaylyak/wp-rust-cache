<?php
// Classic-mode endpoint. ?op=reset | incr | rw | get
$g  = wp_rust_cache_group( 'frankenphp', 'test' );
$op = $_GET['op'] ?? 'incr';
switch ( $op ) {
	case 'reset':
		echo wp_rust_cache_set( $g, 0, 'counter', 0 ) ? 'OK' : 'FAIL';
		break;
	case 'incr':
		echo wp_rust_cache_incr( $g, 0, 'counter', 1 ) === false ? 'FAIL' : 'OK';
		break;
	case 'get':
		echo wp_rust_cache_get( $g, 0, 'counter' );
		break;
	case 'rw':
		// Each request writes an array derived from its key, reads a random
		// key back and checks it is exactly what its writer stored.
		$k = random_int( 0, 499 );
		$v = array( 'id' => $k, 'list' => range( 0, $k % 50 ), 'obj' => (object) array( 'k' => "v$k" ) );
		wp_rust_cache_set( $g, 0, "rw$k", $v );
		$r     = random_int( 0, 499 );
		$found = null;
		$got   = wp_rust_cache_get( $g, 0, "rw$r", $found );
		if ( ! $found ) {
			echo 'OK';
		} elseif ( $got['id'] === $r && $got['list'] === range( 0, $r % 50 ) && $got['obj']->k === "v$r" ) {
			echo 'OK';
		} else {
			echo 'FAIL ' . json_encode( $got );
		}
		break;
}
