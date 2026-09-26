<?php
/**
 * Semantic parity with WordPress core.
 *
 *   php parity.php core <path to wp-includes> [multisite]
 *   php -d extension=... parity.php rust <path to object-cache.php> [multisite]
 *
 * Runs the same operations against core's WP_Object_Cache and against the
 * drop-in, and prints every return value, `$found` flag, hit/miss counter
 * and `_doing_it_wrong` notice as JSON. `run-parity.sh` diffs the two.
 *
 * For the drop-in it then dumps every touched key from a *new* instance
 * (a new request): persistent groups must still hold exactly what core
 * holds at the end of the request; non-persistent groups must be empty.
 */

$impl      = $argv[1];
$source    = $argv[2];
$multisite = isset( $argv[3] ) && 'multisite' === $argv[3];

// Core's cache.php loads ABSPATH . WPINC . '/class-wp-object-cache.php'.
define( 'ABSPATH', ( 'core' === $impl ? dirname( $source ) : __DIR__ ) . '/' );
define( 'WPINC', basename( $source ) );
define( 'KB_IN_BYTES', 1024 );
define( 'WP_CACHE_KEY_SALT', 'parity-' . ( $multisite ? 'ms' : 'single' ) );

$GLOBALS['notices']        = array();
$GLOBALS['suspend']        = false;
$GLOBALS['current_blog']   = 1;
$GLOBALS['multisite_mode'] = $multisite;

function is_multisite() {
	return $GLOBALS['multisite_mode'];
}
function get_current_blog_id() {
	return $GLOBALS['current_blog'];
}
function _doing_it_wrong( $function, $message, $version ) {
	$GLOBALS['notices'][] = "$function: $message";
}
function __( $s ) {
	return $s;
}
function wp_load_translations_early() {
}
function wp_suspend_cache_addition( $suspend = null ) {
	return $GLOBALS['suspend'];
}
function _deprecated_function() {
}
function esc_html( $s ) {
	return $s;
}

if ( 'core' === $impl ) {
	require $source . '/cache.php';
} else {
	require $source;
}

wp_cache_init();
wp_cache_add_global_groups( array( 'users', 'site-options', 'glob' ) );
wp_cache_add_non_persistent_groups( array( 'counts', 'np' ) );
if ( 'rust' === $impl ) {
	if ( ! $GLOBALS['wp_object_cache']->is_persistent() ) {
		fwrite( STDERR, "drop-in is not using shared memory\n" );
		exit( 2 );
	}
	wp_cache_flush();
}

class Thing {
	public $name;
	public $list = array();
	public function __construct( $name ) {
		$this->name = $name;
	}
}

function enc( $v ) {
	if ( is_object( $v ) ) {
		return array( 'object' => get_class( $v ), 'props' => enc( get_object_vars( $v ) ) );
	}
	if ( is_array( $v ) ) {
		$out = array();
		foreach ( $v as $k => $x ) {
			$out[] = array( gettype( $k ), $k, enc( $x ) );
		}
		return array( 'array' => $out );
	}
	return array( gettype( $v ), $v );
}

$log     = array();
$touched = array();
function r( $label, $value, $extra = null ) {
	global $log;
	$log[] = array( $label, enc( $value ), $extra );
}
function t( $key, $group = '' ) {
	global $touched;
	$touched[ ( $group ?: 'default' ) . '|' . $key ] = array( $key, $group ?: 'default', $GLOBALS['current_blog'] );
}

// ---- basic ----------------------------------------------------------------
t( 'k1' );
r( 'set k1', wp_cache_set( 'k1', 'v1' ) );
$found = null;
r( 'get k1', wp_cache_get( 'k1', '', false, $found ), $found );
$found = null;
r( 'get missing', wp_cache_get( 'missing', 'default', false, $found ), $found );
t( 'k2' );
r( 'set false', wp_cache_set( 'k2', false ) );
$found = null;
r( 'get false', wp_cache_get( 'k2', 'default', false, $found ), $found );
t( 'knull' );
r( 'set null', wp_cache_set( 'knull', null ) );
$found = null;
r( 'get null', wp_cache_get( 'knull', 'default', false, $found ), $found );
r( 'set force-get', wp_cache_get( 'k1', 'default', true ) );

// ---- keys -----------------------------------------------------------------
t( 5 );
r( 'set int key', wp_cache_set( 5, 'five' ) );
r( 'get "5"', wp_cache_get( '5' ) );
r( 'get 5', wp_cache_get( 5 ) );
r( 'set empty key', wp_cache_set( '', 'x' ) );
r( 'set blank key', wp_cache_set( '  ', 'x' ) );
r( 'get null key', wp_cache_get( null ) );
r( 'get float key', wp_cache_get( 1.5 ) );
r( 'get array key', wp_cache_get( array() ) );

// ---- add / replace ----------------------------------------------------------
t( 'k3' );
r( 'add existing', wp_cache_add( 'k1', 'x' ) );
r( 'add new', wp_cache_add( 'k3', array( 'a' => 1 ) ) );
r( 'get k3', wp_cache_get( 'k3' ) );
t( 'k4' );
r( 'replace missing', wp_cache_replace( 'k4', 'x' ) );
r( 'replace existing', wp_cache_replace( 'k3', 'y' ) );
r( 'get replaced', wp_cache_get( 'k3' ) );
$GLOBALS['suspend'] = true;
t( 'k5' );
r( 'add while suspended', wp_cache_add( 'k5', 'x' ) );
$GLOBALS['suspend'] = false;
r( 'get k5', wp_cache_get( 'k5' ) );

// ---- objects are copies ----------------------------------------------------
t( 'obj' );
$o = new Thing( 'first' );
r( 'set obj', wp_cache_set( 'obj', $o ) );
$o->name = 'mutated after set';
$got     = wp_cache_get( 'obj' );
r( 'get obj', $got );
$got->name = 'mutated after get';
r( 'get obj again', wp_cache_get( 'obj' ) );
t( 'std' );
wp_cache_set( 'std', (object) array( 'ID' => 3 ) );
r( 'get std', wp_cache_get( 'std' ) );

// ---- incr / decr -----------------------------------------------------------
t( 'n' );
r( 'incr missing', wp_cache_incr( 'n' ) );
wp_cache_set( 'n', 5 );
r( 'incr', wp_cache_incr( 'n', 3 ) );
r( 'decr below 0', wp_cache_decr( 'n', 10 ) );
r( 'incr negative', wp_cache_incr( 'n', -2 ) );
t( 's' );
wp_cache_set( 's', '10' );
r( 'incr numeric string', wp_cache_incr( 's' ) );
t( 'f' );
wp_cache_set( 'f', 1.5 );
r( 'incr float', wp_cache_incr( 'f' ) );
t( 'x' );
wp_cache_set( 'x', 'abc' );
r( 'incr non-numeric', wp_cache_incr( 'x', 2 ) );
r( 'decr missing', wp_cache_decr( 'nothing' ) );
r( 'incr string offset', wp_cache_incr( 'n', '4' ) );

// ---- delete ------------------------------------------------------------------
r( 'delete', wp_cache_delete( 'k1' ) );
r( 'delete again', wp_cache_delete( 'k1' ) );
r( 'get deleted', wp_cache_get( 'k1' ) );
r( 'delete invalid key', wp_cache_delete( '' ) );

// ---- bulk ----------------------------------------------------------------------
foreach ( array( 'm1', 'm2', 9, 'm3' ) as $k ) {
	t( $k, 'bulk' );
}
r( 'set_multiple', wp_cache_set_multiple( array( 'm1' => 1, 'm2' => array( 2 ), 9 => 'nine' ), 'bulk' ) );
r( 'get_multiple', wp_cache_get_multiple( array( 'm1', 'nope', 'm2', 9 ), 'bulk' ) );
r( 'add_multiple', wp_cache_add_multiple( array( 'm1' => 'x', 'm3' => 'three' ), 'bulk' ) );
r( 'delete_multiple', wp_cache_delete_multiple( array( 'm2', 'nope' ), 'bulk' ) );
r( 'get_multiple after', wp_cache_get_multiple( array( 'm1', 'm2', 'm3' ), 'bulk' ) );
r( 'get_multiple force', wp_cache_get_multiple( array( 'm1', 'm3' ), 'bulk', true ) );

// ---- groups --------------------------------------------------------------------
t( 'g1', 'grp' );
t( 'g2', 'grp' );
t( 'o1', 'other' );
wp_cache_set( 'g1', 'a', 'grp' );
wp_cache_set( 'g2', 'b', 'grp' );
wp_cache_set( 'o1', 'c', 'other' );
r( 'flush_group', wp_cache_flush_group( 'grp' ) );
r( 'get flushed group', wp_cache_get( 'g1', 'grp' ) );
r( 'get other group', wp_cache_get( 'o1', 'other' ) );
t( 'd', 'default' );
r( 'empty group is default', wp_cache_set( 'd', 'dv', '' ) );
r( 'get via default', wp_cache_get( 'd', 'default' ) );
r( 'supports', array( wp_cache_supports( 'flush_group' ), wp_cache_supports( 'get_multiple' ), wp_cache_supports( 'bogus' ) ) );

// ---- non-persistent and global groups -----------------------------------------------
t( 'c', 'counts' );
r( 'set non-persistent', wp_cache_set( 'c', 7, 'counts' ) );
r( 'get non-persistent', wp_cache_get( 'c', 'counts' ) );
r( 'incr non-persistent', wp_cache_incr( 'c', 1, 'counts' ) );
t( 'u1', 'users' );
r( 'set global', wp_cache_set( 'u1', 'alice', 'users' ) );

// ---- multisite ---------------------------------------------------------------------
if ( $multisite ) {
	t( 'post', 'posts' );
	wp_cache_set( 'post', 'blog1', 'posts' );
	$GLOBALS['current_blog'] = 2;
	wp_cache_switch_to_blog( 2 );
	r( 'blog 2 does not see blog 1', wp_cache_get( 'post', 'posts' ) );
	t( 'post', 'posts' );
	wp_cache_set( 'post', 'blog2', 'posts' );
	r( 'blog 2 sees global', wp_cache_get( 'u1', 'users' ) );
	$GLOBALS['current_blog'] = 1;
	wp_cache_switch_to_blog( 1 );
	r( 'back on blog 1', wp_cache_get( 'post', 'posts' ) );
	r( 'flush_group across blogs', wp_cache_flush_group( 'posts' ) );
	r( 'blog 1 after flush_group', wp_cache_get( 'post', 'posts' ) );
	wp_cache_set( 'post', 'blog1-again', 'posts' );
}

r( 'counters', array( $GLOBALS['wp_object_cache']->cache_hits, $GLOBALS['wp_object_cache']->cache_misses ) );
r( 'notices', $GLOBALS['notices'] );

// ---- state at the end of the request ---------------------------------------------------
$non_persistent = array( 'counts' => true, 'np' => true );
$dump           = array();
if ( 'rust' === $impl ) {
	// Same state seen from a new request.
	wp_cache_init();
	wp_cache_add_global_groups( array( 'users', 'site-options', 'glob' ) );
	wp_cache_add_non_persistent_groups( array( 'counts', 'np' ) );
}
foreach ( $touched as $id => $t ) {
	list( $key, $group, $blog ) = $t;
	if ( $multisite ) {
		$GLOBALS['current_blog'] = $blog;
		wp_cache_switch_to_blog( $blog );
	}
	$found = null;
	$value = wp_cache_get( $key, $group, false, $found );
	if ( isset( $non_persistent[ $group ] ) ) {
		continue; // core keeps them for the request, a new request must not
	}
	$dump[ "$blog|$id" ] = array( $found, enc( $value ) );
}
if ( 'rust' === $impl ) {
	$found = null;
	wp_cache_get( 'c', 'counts', false, $found );
	$dump['non-persistent group not shared'] = array( false === $found );
} else {
	$dump['non-persistent group not shared'] = array( true );
}

// Finally: flush empties everything for this install.
wp_cache_flush();
$found = null;
wp_cache_get( 'u1', 'users', false, $found );
$dump['flush empties'] = array( false === $found );

echo json_encode( array( 'log' => $log, 'dump' => $dump ), JSON_PRETTY_PRINT ), "\n";
