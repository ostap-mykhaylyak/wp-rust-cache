<?php
/**
 * Tests of the wp_rust_cache extension functions.
 *   php -d extension=wp_rust_cache.so -d wp_rust_cache.config=tests/php/test.toml tests/php/extension_test.php
 */

$failures = 0;
$checks   = 0;

function check( $cond, $what ) {
	global $failures, $checks;
	++$checks;
	if ( ! $cond ) {
		++$failures;
		fwrite( STDERR, "FAIL: $what\n" );
		$bt = debug_backtrace();
		fwrite( STDERR, '  at line ' . $bt[0]['line'] . "\n" );
	}
}

class Point {
	public $x;
	public $y;
	public function __construct( $x, $y ) {
		$this->x = $x;
		$this->y = $y;
	}
}

class WithMagic {
	public $data = array();
	public $woken = false;
	public function __serialize(): array {
		return array( 'd' => $this->data );
	}
	public function __unserialize( array $a ): void {
		$this->data  = $a['d'];
		$this->woken = true;
	}
}

check( wp_rust_cache_available(), 'segment available: ' . json_encode( wp_rust_cache_info() ) );
wp_rust_cache_flush_all();

$ns = 'test-' . getmypid();
$g  = wp_rust_cache_group( $ns, 'default' );
check( is_int( $g ), 'group id' );
check( wp_rust_cache_group( $ns, 'default' ) === $g, 'group id is stable' );
$g2 = wp_rust_cache_group( $ns, 'other' );
check( $g2 !== $g, 'distinct groups' );

// Round trip of every type WordPress stores.
$values = array(
	'string'   => 'hello',
	'empty'    => '',
	'binary'   => "a\0b\xff",
	'int'      => 42,
	'negative' => -7,
	'big'      => PHP_INT_MAX,
	'float'    => 3.25,
	'true'     => true,
	'false'    => false,
	'null'     => null,
	'array'    => array( 'a' => 1, 'b' => array( 2, 3 ), 5 => 'five' ),
	'list'     => array( 1, 2, 3 ),
	'stdclass' => (object) array( 'ID' => 5, 'post_title' => 'x' ),
	'point'    => new Point( 1, 2 ),
	'numeric'  => '0123',
);
foreach ( $values as $k => $v ) {
	check( wp_rust_cache_set( $g, 0, $k, $v ), "set $k" );
	$found = null;
	$got   = wp_rust_cache_get( $g, 0, $k, $found );
	check( true === $found, "found $k" );
	if ( is_object( $v ) ) {
		check( $got == $v && get_class( $got ) === get_class( $v ), "object $k round-trips" );
		check( $got !== $v, "object $k is a copy" );
	} else {
		check( $got === $v, "value $k round-trips: " . var_export( $got, true ) );
	}
}

// Miss vs stored false.
$found = null;
check( false === wp_rust_cache_get( $g, 0, 'nope', $found ) && false === $found, 'miss sets found=false' );
wp_rust_cache_set( $g, 0, 'f', false );
$found = null;
check( false === wp_rust_cache_get( $g, 0, 'f', $found ) && true === $found, 'stored false is found' );

// Int and string keys are the same key.
wp_rust_cache_set( $g, 0, 5, 'five' );
check( 'five' === wp_rust_cache_get( $g, 0, '5' ), 'int key == string key' );

// Magic serialization.
$m       = new WithMagic();
$m->data = array( 1, 2 );
wp_rust_cache_set( $g, 0, 'magic', $m );
$got = wp_rust_cache_get( $g, 0, 'magic' );
check( $got instanceof WithMagic && $got->woken && $got->data === array( 1, 2 ), '__serialize/__unserialize' );

// A closure cannot be serialized: set fails quietly and removes the old copy.
wp_rust_cache_set( $g, 0, 'cl', 'old' );
$r = wp_rust_cache_set( $g, 0, 'cl', function () {} );
check( false === $r, 'closure set returns false' );
$found = null;
wp_rust_cache_get( $g, 0, 'cl', $found );
check( false === $found, 'closure set removed the older value' );

// add / replace.
check( wp_rust_cache_add( $g, 0, 'a1', 1 ), 'add new' );
check( ! wp_rust_cache_add( $g, 0, 'a1', 2 ), 'add existing fails' );
check( ! wp_rust_cache_replace( $g, 0, 'r1', 1 ), 'replace missing fails' );
check( wp_rust_cache_replace( $g, 0, 'a1', 3 ) && 3 === wp_rust_cache_get( $g, 0, 'a1' ), 'replace existing' );

// incr / decr.
check( false === wp_rust_cache_incr( $g, 0, 'counter', 1 ), 'incr missing' );
wp_rust_cache_set( $g, 0, 'counter', 10 );
check( 15 === wp_rust_cache_incr( $g, 0, 'counter', 5 ), 'incr' );
check( 0 === wp_rust_cache_incr( $g, 0, 'counter', -100 ), 'decr floors at 0' );
wp_rust_cache_set( $g, 0, 'sc', '7' );
check( 8 === wp_rust_cache_incr( $g, 0, 'sc', 1 ), 'incr numeric string gives int' );
wp_rust_cache_set( $g, 0, 'fc', 1.5 );
check( 2.5 === wp_rust_cache_incr( $g, 0, 'fc', 1 ), 'incr float stays float' );
wp_rust_cache_set( $g, 0, 'xc', 'abc' );
check( 2 === wp_rust_cache_incr( $g, 0, 'xc', 2 ), 'incr non-numeric starts at 0' );

// Bulk.
$r = wp_rust_cache_set_multiple( $g, 0, array( 'm1' => 'one', 'm2' => array( 2 ), 7 => 'seven' ) );
check( array( 'm1' => true, 'm2' => true, 7 => true ) === $r, 'set_multiple results: ' . json_encode( $r ) );
$r = wp_rust_cache_get_multiple( $g, 0, array( 'm1', 'm2', 'missing', '7' ) );
check( array( 'm1' => 'one', 'm2' => array( 2 ), 7 => 'seven' ) === $r, 'get_multiple: ' . json_encode( $r ) );
$r = wp_rust_cache_delete_multiple( $g, 0, array( 'm1', 'missing' ) );
check( array( 'm1' => true, 'missing' => false ) === $r, 'delete_multiple' );

// Delete.
check( wp_rust_cache_delete( $g, 0, 'm2' ), 'delete existing' );
check( ! wp_rust_cache_delete( $g, 0, 'm2' ), 'delete missing' );

// Blogs and groups are separate keyspaces.
wp_rust_cache_set( $g, 1, 'k', 'blog1' );
wp_rust_cache_set( $g, 2, 'k', 'blog2' );
wp_rust_cache_set( $g2, 1, 'k', 'other' );
check( 'blog1' === wp_rust_cache_get( $g, 1, 'k' ), 'blog 1' );
check( 'blog2' === wp_rust_cache_get( $g, 2, 'k' ), 'blog 2' );
check( 'other' === wp_rust_cache_get( $g2, 1, 'k' ), 'other group' );

// flush_group empties one group, all blogs.
wp_rust_cache_flush_group( $g );
check( false === wp_rust_cache_get( $g, 1, 'k' ) && false === wp_rust_cache_get( $g, 2, 'k' ), 'flush_group' );
check( 'other' === wp_rust_cache_get( $g2, 1, 'k' ), 'flush_group leaves other groups' );

// flush_namespace empties this install only.
$other_ns = wp_rust_cache_group( $ns . '-b', 'default' );
wp_rust_cache_set( $other_ns, 0, 'k', 'b' );
wp_rust_cache_flush_namespace( $ns );
check( false === wp_rust_cache_get( $g2, 1, 'k' ), 'flush_namespace' );
check( 'b' === wp_rust_cache_get( $other_ns, 0, 'k' ), 'flush_namespace leaves other installs' );

// TTL.
wp_rust_cache_set( $g, 0, 'ttl', 'v', 1 );
check( 'v' === wp_rust_cache_get( $g, 0, 'ttl' ), 'ttl before expiry' );
usleep( 2100000 );
check( false === wp_rust_cache_get( $g, 0, 'ttl' ), 'ttl expired' );

// Large value: stored when under max_item_size, refused (and old copy removed) above.
$big = str_repeat( 'x', 512 * 1024 );
check( wp_rust_cache_set( $g, 0, 'big', $big ) && wp_rust_cache_get( $g, 0, 'big' ) === $big, '512 KB value' );
$huge = str_repeat( 'y', 64 * 1024 * 1024 );
check( ! wp_rust_cache_set( $g, 0, 'big', $huge ), '64 MB value refused' );
check( false === wp_rust_cache_get( $g, 0, 'big' ), 'refused value removed the old one' );
unset( $huge );

// 100 processes incrementing the same key: the result must be exact.
if ( function_exists( 'pcntl_fork' ) ) {
	wp_rust_cache_set( $g, 0, 'race', 0 );
	$pids = array();
	for ( $i = 0; $i < 100; $i++ ) {
		$pid = pcntl_fork();
		if ( 0 === $pid ) {
			for ( $j = 0; $j < 200; $j++ ) {
				wp_rust_cache_incr( $g, 0, 'race', 1 );
			}
			exit( 0 );
		}
		$pids[] = $pid;
	}
	foreach ( $pids as $pid ) {
		pcntl_waitpid( $pid, $status );
	}
	$v = wp_rust_cache_get( $g, 0, 'race' );
	check( 20000 === $v, "100 processes × 200 incr = 20000, got $v" );
}

// Group ids never outlive the registry that issued them: after the registry
// is emptied (more than 20 000 groups, at a request start), an old id fails
// instead of naming whatever group now sits at its index.
$old = wp_rust_cache_group( $ns, 'before-reset' );
wp_rust_cache_set( $old, 0, 'k', 'old group' );
for ( $i = 0; $i < 20050; $i++ ) {
	wp_rust_cache_group( $ns, "product_$i" );
}
wp_rust_cache_available(); // request start: the registry is emptied
$other = wp_rust_cache_group( $ns, 'after-reset' ); // takes index 0 again
wp_rust_cache_set( $other, 0, 'k', 'new group' );
$found = null;
check( false === wp_rust_cache_get( $old, 0, 'k', $found ) && false === $found, 'stale group id is refused' );
check( ! wp_rust_cache_set( $old, 0, 'k', 'x' ), 'stale group id cannot write' );
check( 'new group' === wp_rust_cache_get( $other, 0, 'k' ), 'the new group is untouched' );
check( 'old group' === wp_rust_cache_get( wp_rust_cache_group( $ns, 'before-reset' ), 0, 'k' ), 'the old group is reachable by name' );

$s = wp_rust_cache_stats();
check( is_array( $s ) && $s['hits'] > 0 && $s['entries'] > 0, 'stats' );

echo "$checks checks, $failures failures\n";
exit( $failures ? 1 : 0 );
