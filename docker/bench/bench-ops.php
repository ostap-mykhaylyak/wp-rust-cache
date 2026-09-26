<?php
/**
 * wp_cache_* latency inside a real WordPress, through whatever drop-in is
 * installed. Run by bench-ops.sh as `wp eval-file`, one process per worker.
 *
 * A "request" starts with an empty in-request cache,
 * then performs the lookups of a WooCommerce page view with real values:
 * WP_Post objects, post-meta arrays, terms, users, alloptions, a session.
 * Every miss is followed by a set, as WordPress does after a database read.
 *
 * Env: BENCH_BARRIER (directory), BENCH_WORKERS, BENCH_SECONDS, BENCH_OUT, BENCH_SEED.
 */

$seconds = (float) getenv( 'BENCH_SECONDS' );
$out     = getenv( 'BENCH_OUT' );
mt_srand( (int) getenv( 'BENCH_SEED' ) );

function zipf_table( $n, $s ) {
	$cdf = array();
	$sum = 0.0;
	for ( $i = 1; $i <= $n; $i++ ) {
		$sum  += 1.0 / pow( $i, $s );
		$cdf[] = $sum;
	}
	foreach ( $cdf as &$c ) {
		$c /= $sum;
	}
	return $cdf;
}

function zipf( $cdf ) {
	$u  = mt_rand() / mt_getrandmax();
	$lo = 0;
	$hi = count( $cdf ) - 1;
	while ( $lo < $hi ) {
		$mid = ( $lo + $hi ) >> 1;
		if ( $cdf[ $mid ] < $u ) {
			$lo = $mid + 1;
		} else {
			$hi = $mid;
		}
	}
	return $lo;
}

function bucket( $ns ) {
	if ( $ns < 16 ) {
		return $ns;
	}
	$e = (int) floor( log( $ns, 2 ) );
	return 16 + ( $e - 4 ) * 8 + ( ( $ns >> ( $e - 3 ) ) & 7 );
}

// Real values, loaded before the clock starts.
$post_ids = get_posts( array( 'numberposts' => 3000, 'post_type' => 'post', 'fields' => 'ids', 'orderby' => 'ID' ) );
$product_ids = get_posts( array( 'numberposts' => 1000, 'post_type' => 'product', 'fields' => 'ids', 'orderby' => 'ID' ) );
$term_ids = get_terms( array( 'taxonomy' => array( 'category', 'post_tag' ), 'hide_empty' => false, 'fields' => 'ids' ) );
$user_ids = get_users( array( 'fields' => 'ID', 'number' => 500 ) );

$posts = array();
$meta  = array();
foreach ( array_merge( $post_ids, $product_ids ) as $id ) {
	$posts[ $id ] = get_post( $id );
	$meta[ $id ]  = get_post_meta( $id );
}
$terms = array();
foreach ( $term_ids as $id ) {
	$terms[ $id ] = get_term( $id );
}
$users = array();
foreach ( $user_ids as $id ) {
	$users[ $id ] = get_userdata( $id )->data;
}
$alloptions = wp_load_alloptions();
$session    = array( 'cart' => serialize( array_fill( 0, 3, array( 'product_id' => 1, 'quantity' => 2 ) ) ), 'customer' => serialize( array( 'id' => 0, 'country' => 'IT' ) ) );

$zp = zipf_table( count( $post_ids ), 1.0 );
$zr = zipf_table( count( $product_ids ), 1.0 );
$zt = zipf_table( count( $term_ids ), 0.9 );
$zu = zipf_table( count( $user_ids ), 1.0 );
$zv = zipf_table( 50000, 0.6 );

$get  = array();
$set  = array();
$ops  = 0;
$reqs = 0;

$fetch = function ( $key, $group, $value, $ttl = 0 ) use ( &$get, &$set, &$ops ) {
	$found = null;
	$t     = hrtime( true );
	wp_cache_get( $key, $group, false, $found );
	$d = hrtime( true ) - $t;
	$b = bucket( $d );
	$get[ $b ] = ( $get[ $b ] ?? 0 ) + 1;
	++$ops;
	if ( ! $found ) {
		$t = hrtime( true );
		wp_cache_set( $key, $value, $group, $ttl );
		$d = hrtime( true ) - $t;
		$b = bucket( $d );
		$set[ $b ] = ( $set[ $b ] ?? 0 ) + 1;
		++$ops;
	}
};

// Barrier: loading the values takes long and varies with the worker count,
// so every worker announces it is ready and waits for all the others.
$barrier = getenv( 'BENCH_BARRIER' );
$workers = (int) getenv( 'BENCH_WORKERS' );
touch( "$barrier/ready-" . getenv( 'BENCH_SEED' ) );
while ( count( glob( "$barrier/ready-*" ) ) < $workers ) {
	usleep( 2000 );
}
$deadline = microtime( true ) + $seconds;
while ( microtime( true ) < $deadline ) {
	// A new request. Not wp_cache_flush_runtime(): the Memcached drop-in does
	// not implement it and would keep serving from its in-request array. All
	// three drop-ins keep that array in a public `$cache` property.
	$GLOBALS['wp_object_cache']->cache = array();
	// The Memcached drop-in logs every operation in `group_ops`; a real
	// request discards it at the end, a 10-second process would grow it
	// until the machine runs out of memory (it did, at 32 workers).
	if ( isset( $GLOBALS['wp_object_cache']->group_ops ) ) {
		$GLOBALS['wp_object_cache']->group_ops = array();
	}
	$fetch( 'alloptions', 'options', $alloptions );
	$fetch( 'notoptions', 'options', array() );
	$fetch( 'last_changed', 'posts', microtime() );
	for ( $i = 0; $i < 10; $i++ ) {
		$id = $post_ids[ zipf( $zp ) ];
		$fetch( $id, 'posts', $posts[ $id ] );
		$fetch( $id, 'post_meta', $meta[ $id ] );
	}
	for ( $i = 0; $i < 6; $i++ ) {
		$id = $product_ids[ zipf( $zr ) ];
		$fetch( $id, 'posts', $posts[ $id ] );
		$fetch( $id, 'post_meta', $meta[ $id ] );
	}
	for ( $i = 0; $i < 5; $i++ ) {
		$id = $term_ids[ zipf( $zt ) ];
		$fetch( $id, 'terms', $terms[ $id ] );
	}
	for ( $i = 0; $i < 2; $i++ ) {
		$id = $user_ids[ zipf( $zu ) ];
		$fetch( $id, 'users', $users[ $id ] );
	}
	$fetch( 'wc_session_' . zipf( $zv ), 'wc_session_id', $session, 172800 );
	if ( 0 === mt_rand( 0, 49 ) ) {
		$id = $post_ids[ zipf( $zp ) ];
		$t  = hrtime( true );
		wp_cache_set( $id, $posts[ $id ], 'posts' );
		wp_cache_set( 'last_changed', microtime(), 'posts' );
		$b = bucket( ( hrtime( true ) - $t ) >> 1 );
		$set[ $b ] = ( $set[ $b ] ?? 0 ) + 2;
		$ops += 2;
	}
	++$reqs;
}

file_put_contents( $out, json_encode( array( 'get' => $get, 'set' => $set, 'ops' => $ops, 'requests' => $reqs ) ) );
