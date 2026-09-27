<?php
/**
 * Plugin Name: wp-rust-cache
 * Description: Persistent object cache in shared memory, served by the wp_rust_cache PHP extension. No Redis, no Memcached, no network.
 * Version: 0.1.1
 * License: MIT
 *
 * Install as wp-content/object-cache.php. Without the extension (or when the
 * shared-memory segment cannot be used) it behaves exactly like WordPress's
 * built-in, non-persistent cache, so the site keeps working.
 *
 * Optional constants (wp-config.php):
 *   WP_RUST_CACHE_DISABLED   true → never use shared memory
 *   WP_RUST_CACHE_NAMESPACE  separates installs sharing one segment
 *                            (default: WP_CACHE_KEY_SALT, else DB host/name/prefix)
 */

defined( 'ABSPATH' ) || exit;

define( 'WP_RUST_CACHE_DROPIN_VERSION', '0.1.1' );

function wp_cache_init() {
	// phpcs:ignore WordPress.WP.GlobalVariablesOverride.Prohibited
	$GLOBALS['wp_object_cache'] = new WP_Object_Cache();
}

function wp_cache_add( $key, $data, $group = '', $expire = 0 ) {
	global $wp_object_cache;
	return $wp_object_cache->add( $key, $data, $group, (int) $expire );
}

function wp_cache_add_multiple( array $data, $group = '', $expire = 0 ) {
	global $wp_object_cache;
	return $wp_object_cache->add_multiple( $data, $group, (int) $expire );
}

function wp_cache_replace( $key, $data, $group = '', $expire = 0 ) {
	global $wp_object_cache;
	return $wp_object_cache->replace( $key, $data, $group, (int) $expire );
}

function wp_cache_set( $key, $data, $group = '', $expire = 0 ) {
	global $wp_object_cache;
	return $wp_object_cache->set( $key, $data, $group, (int) $expire );
}

function wp_cache_set_multiple( array $data, $group = '', $expire = 0 ) {
	global $wp_object_cache;
	return $wp_object_cache->set_multiple( $data, $group, (int) $expire );
}

function wp_cache_get( $key, $group = '', $force = false, &$found = null ) {
	global $wp_object_cache;
	return $wp_object_cache->get( $key, $group, $force, $found );
}

function wp_cache_get_multiple( $keys, $group = '', $force = false ) {
	global $wp_object_cache;
	return $wp_object_cache->get_multiple( $keys, $group, $force );
}

function wp_cache_delete( $key, $group = '' ) {
	global $wp_object_cache;
	return $wp_object_cache->delete( $key, $group );
}

function wp_cache_delete_multiple( array $keys, $group = '' ) {
	global $wp_object_cache;
	return $wp_object_cache->delete_multiple( $keys, $group );
}

function wp_cache_incr( $key, $offset = 1, $group = '' ) {
	global $wp_object_cache;
	return $wp_object_cache->incr( $key, $offset, $group );
}

function wp_cache_decr( $key, $offset = 1, $group = '' ) {
	global $wp_object_cache;
	return $wp_object_cache->decr( $key, $offset, $group );
}

function wp_cache_flush() {
	global $wp_object_cache;
	return $wp_object_cache->flush();
}

function wp_cache_flush_runtime() {
	global $wp_object_cache;
	return $wp_object_cache->flush_runtime();
}

function wp_cache_flush_group( $group ) {
	global $wp_object_cache;
	return $wp_object_cache->flush_group( $group );
}

function wp_cache_supports( $feature ) {
	switch ( $feature ) {
		case 'add_multiple':
		case 'set_multiple':
		case 'get_multiple':
		case 'delete_multiple':
		case 'flush_runtime':
		case 'flush_group':
			return true;
		default:
			return false;
	}
}

function wp_cache_close() {
	return true;
}

function wp_cache_add_global_groups( $groups ) {
	global $wp_object_cache;
	$wp_object_cache->add_global_groups( $groups );
}

function wp_cache_add_non_persistent_groups( $groups ) {
	global $wp_object_cache;
	$wp_object_cache->add_non_persistent_groups( $groups );
}

function wp_cache_switch_to_blog( $blog_id ) {
	global $wp_object_cache;
	$wp_object_cache->switch_to_blog( $blog_id );
}

function wp_cache_reset() {
	_deprecated_function( __FUNCTION__, '3.5.0', 'wp_cache_switch_to_blog()' );
	global $wp_object_cache;
	$wp_object_cache->reset();
}

/**
 * Two levels: the in-request array `$cache` (same layout as core, so code
 * that reads `$wp_object_cache->cache` keeps working) in front of the shared
 * memory segment. Values cross processes as copies, so objects come back as
 * fresh instances, exactly like core's clone-on-read.
 */
#[AllowDynamicProperties]
class WP_Object_Cache {

	/** @var array<string, array<string, mixed>> In-request cache: [group][key]. */
	public $cache = array();

	/** @var int */
	public $cache_hits = 0;

	/** @var int */
	public $cache_misses = 0;

	/** @var int Lookups that reached shared memory. */
	public $persistent_lookups = 0;

	/** @var int Shared-memory lookups that hit. */
	public $persistent_hits = 0;

	/** @var array<string, true> */
	protected $global_groups = array();

	/** @var array<string, true> */
	protected $non_persistent_groups = array();

	/** @var string */
	private $blog_prefix = '';

	/** @var int */
	private $blog_id = 0;

	/** @var bool */
	private $multisite = false;

	/** @var bool Whether shared memory is in use. */
	private $persistent = false;

	/** @var string */
	private $namespace = '';

	/** @var array<string, int|false> Group name → segment group id. */
	private $gids = array();

	public function __construct() {
		$this->multisite   = is_multisite();
		$this->blog_id     = $this->multisite ? (int) get_current_blog_id() : 0;
		$this->blog_prefix = $this->multisite ? $this->blog_id . ':' : '';
		$this->namespace   = self::namespace_name();
		$this->persistent  = ! ( defined( 'WP_RUST_CACHE_DISABLED' ) && WP_RUST_CACHE_DISABLED )
			&& function_exists( 'wp_rust_cache_available' )
			&& wp_rust_cache_available();
	}

	/**
	 * One namespace per WordPress install: installs that share a PHP user
	 * (and so a segment) never see each other's keys, and a flush only
	 * empties this install.
	 */
	public static function namespace_name() {
		if ( defined( 'WP_RUST_CACHE_NAMESPACE' ) && WP_RUST_CACHE_NAMESPACE ) {
			return (string) WP_RUST_CACHE_NAMESPACE;
		}
		if ( defined( 'WP_CACHE_KEY_SALT' ) && WP_CACHE_KEY_SALT ) {
			return (string) WP_CACHE_KEY_SALT;
		}
		$host   = defined( 'DB_HOST' ) ? DB_HOST : '';
		$name   = defined( 'DB_NAME' ) ? DB_NAME : '';
		$prefix = isset( $GLOBALS['table_prefix'] ) ? $GLOBALS['table_prefix'] : '';
		return md5( $host . '|' . $name . '|' . $prefix );
	}

	public function is_persistent() {
		return $this->persistent;
	}

	public function get_namespace() {
		return $this->namespace;
	}

	/**
	 * Segment group id, or false when the group must stay in memory only
	 * (non-persistent group, no extension, full group table).
	 */
	private function gid( $group ) {
		if ( ! $this->persistent || isset( $this->non_persistent_groups[ $group ] ) ) {
			return false;
		}
		if ( ! isset( $this->gids[ $group ] ) ) {
			$this->gids[ $group ] = wp_rust_cache_group( $this->namespace, (string) $group );
		}
		return $this->gids[ $group ];
	}

	/** Blog id stored with the key: 0 for global groups and single sites. */
	private function blog( $group ) {
		return ( $this->multisite && ! isset( $this->global_groups[ $group ] ) ) ? $this->blog_id : 0;
	}

	/** Key in the in-request array, prefixed like core. */
	private function id( $key, $group ) {
		return ( $this->multisite && ! isset( $this->global_groups[ $group ] ) ) ? $this->blog_prefix . $key : $key;
	}

	private function local_exists( $id, $group ) {
		return isset( $this->cache[ $group ] ) && ( isset( $this->cache[ $group ][ $id ] ) || array_key_exists( $id, $this->cache[ $group ] ) );
	}

	/** Same checks and message as core (6.1+). */
	protected function is_valid_key( $key ) {
		if ( is_int( $key ) ) {
			return true;
		}
		if ( is_string( $key ) && '' !== trim( $key ) ) {
			return true;
		}
		$type = gettype( $key );
		if ( ! function_exists( '__' ) ) {
			wp_load_translations_early();
		}
		$message = is_string( $key )
			? __( 'Cache key must not be an empty string.' )
			/* translators: %s: The type of the given cache key. */
			: sprintf( __( 'Cache key must be an integer or a non-empty string, %s given.' ), $type );
		_doing_it_wrong(
			sprintf( '%s::%s', __CLASS__, debug_backtrace( DEBUG_BACKTRACE_IGNORE_ARGS, 2 )[1]['function'] ),
			$message,
			'6.1.0'
		);
		return false;
	}

	public function add( $key, $data, $group = 'default', $expire = 0 ) {
		if ( function_exists( 'wp_suspend_cache_addition' ) && wp_suspend_cache_addition() ) {
			return false;
		}
		if ( ! $this->is_valid_key( $key ) ) {
			return false;
		}
		if ( empty( $group ) ) {
			$group = 'default';
		}
		$id = $this->id( $key, $group );
		if ( $this->local_exists( $id, $group ) ) {
			return false;
		}
		$gid = $this->gid( $group );
		if ( false !== $gid && ! wp_rust_cache_add( $gid, $this->blog( $group ), $key, $data, (int) $expire ) ) {
			return false;
		}
		$this->cache[ $group ][ $id ] = is_object( $data ) ? clone $data : $data;
		return true;
	}

	public function add_multiple( array $data, $group = '', $expire = 0 ) {
		$values = array();
		foreach ( $data as $key => $value ) {
			$values[ $key ] = $this->add( $key, $value, $group, $expire );
		}
		return $values;
	}

	public function replace( $key, $data, $group = 'default', $expire = 0 ) {
		if ( ! $this->is_valid_key( $key ) ) {
			return false;
		}
		if ( empty( $group ) ) {
			$group = 'default';
		}
		$id = $this->id( $key, $group );
		if ( $this->local_exists( $id, $group ) ) {
			return $this->set( $key, $data, $group, (int) $expire );
		}
		$gid = $this->gid( $group );
		if ( false === $gid || ! wp_rust_cache_replace( $gid, $this->blog( $group ), $key, $data, (int) $expire ) ) {
			return false;
		}
		$this->cache[ $group ][ $id ] = is_object( $data ) ? clone $data : $data;
		return true;
	}

	public function set( $key, $data, $group = 'default', $expire = 0 ) {
		if ( ! $this->is_valid_key( $key ) ) {
			return false;
		}
		if ( empty( $group ) ) {
			$group = 'default';
		}
		if ( is_object( $data ) ) {
			$data = clone $data;
		}
		$this->cache[ $group ][ $this->id( $key, $group ) ] = $data;
		$gid = $this->gid( $group );
		if ( false !== $gid ) {
			// A value that cannot be stored (too large, a closure) stays in
			// this request only; the extension removes any older copy.
			wp_rust_cache_set( $gid, $this->blog( $group ), $key, $data, (int) $expire );
		}
		return true;
	}

	public function set_multiple( array $data, $group = '', $expire = 0 ) {
		if ( empty( $group ) ) {
			$group = 'default';
		}
		$values = array();
		$store  = array();
		foreach ( $data as $key => $value ) {
			if ( ! $this->is_valid_key( $key ) ) {
				$values[ $key ] = false;
				continue;
			}
			if ( is_object( $value ) ) {
				$value = clone $value;
			}
			$this->cache[ $group ][ $this->id( $key, $group ) ] = $value;
			$store[ $key ]  = $value;
			$values[ $key ] = true;
		}
		$gid = $this->gid( $group );
		if ( false !== $gid && $store ) {
			wp_rust_cache_set_multiple( $gid, $this->blog( $group ), $store, (int) $expire );
		}
		return $values;
	}

	public function get( $key, $group = 'default', $force = false, &$found = null ) {
		if ( ! $this->is_valid_key( $key ) ) {
			return false;
		}
		if ( empty( $group ) ) {
			$group = 'default';
		}
		$id  = $this->id( $key, $group );
		$gid = $this->gid( $group );
		if ( ( ! $force || false === $gid ) && $this->local_exists( $id, $group ) ) {
			$found = true;
			++$this->cache_hits;
			$value = $this->cache[ $group ][ $id ];
			return is_object( $value ) ? clone $value : $value;
		}
		if ( false === $gid ) {
			$found = false;
			++$this->cache_misses;
			return false;
		}
		++$this->persistent_lookups;
		$value = wp_rust_cache_get( $gid, $this->blog( $group ), $key, $found );
		if ( ! $found ) {
			++$this->cache_misses;
			return false;
		}
		++$this->cache_hits;
		++$this->persistent_hits;
		$this->cache[ $group ][ $id ] = $value;
		return is_object( $value ) ? clone $value : $value;
	}

	public function get_multiple( $keys, $group = 'default', $force = false ) {
		if ( empty( $group ) ) {
			$group = 'default';
		}
		$gid     = $this->gid( $group );
		$values  = array();
		$missing = array();
		foreach ( $keys as $key ) {
			if ( ! $this->is_valid_key( $key ) ) {
				$values[ $key ] = false;
				continue;
			}
			$id = $this->id( $key, $group );
			if ( ( ! $force || false === $gid ) && $this->local_exists( $id, $group ) ) {
				++$this->cache_hits;
				$value          = $this->cache[ $group ][ $id ];
				$values[ $key ] = is_object( $value ) ? clone $value : $value;
			} elseif ( false === $gid ) {
				++$this->cache_misses;
				$values[ $key ] = false;
			} else {
				$missing[]      = $key;
				$values[ $key ] = false; // keeps the caller's key order
			}
		}
		if ( $missing ) {
			$this->persistent_lookups += count( $missing );
			$fetched = wp_rust_cache_get_multiple( $gid, $this->blog( $group ), $missing );
			foreach ( $missing as $key ) {
				if ( array_key_exists( $key, $fetched ) ) {
					$value = $fetched[ $key ];
					++$this->cache_hits;
					++$this->persistent_hits;
					$this->cache[ $group ][ $this->id( $key, $group ) ] = $value;
					$values[ $key ] = is_object( $value ) ? clone $value : $value;
				} else {
					++$this->cache_misses;
				}
			}
		}
		return $values;
	}

	public function delete( $key, $group = 'default', $deprecated = false ) {
		if ( ! $this->is_valid_key( $key ) ) {
			return false;
		}
		if ( empty( $group ) ) {
			$group = 'default';
		}
		$id      = $this->id( $key, $group );
		$existed = $this->local_exists( $id, $group );
		unset( $this->cache[ $group ][ $id ] );
		$gid = $this->gid( $group );
		if ( false !== $gid ) {
			return wp_rust_cache_delete( $gid, $this->blog( $group ), $key ) || $existed;
		}
		return $existed;
	}

	public function delete_multiple( array $keys, $group = '' ) {
		if ( empty( $group ) ) {
			$group = 'default';
		}
		$values = array();
		$valid  = array();
		foreach ( $keys as $key ) {
			if ( ! $this->is_valid_key( $key ) ) {
				$values[ $key ] = false;
				continue;
			}
			$id             = $this->id( $key, $group );
			$values[ $key ] = $this->local_exists( $id, $group );
			unset( $this->cache[ $group ][ $id ] );
			$valid[] = $key;
		}
		$gid = $this->gid( $group );
		if ( false !== $gid && $valid ) {
			foreach ( wp_rust_cache_delete_multiple( $gid, $this->blog( $group ), $valid ) as $key => $deleted ) {
				$values[ $key ] = $values[ $key ] || $deleted;
			}
		}
		return $values;
	}

	public function incr( $key, $offset = 1, $group = 'default' ) {
		return $this->add_offset( $key, (int) $offset, $group );
	}

	public function decr( $key, $offset = 1, $group = 'default' ) {
		return $this->add_offset( $key, -(int) $offset, $group );
	}

	/**
	 * Core semantics: false when missing, non-numeric counts as 0, floored
	 * at 0. With shared memory the arithmetic runs atomically inside the
	 * segment, so concurrent workers never lose an update.
	 */
	private function add_offset( $key, $offset, $group ) {
		if ( ! $this->is_valid_key( $key ) ) {
			return false;
		}
		if ( empty( $group ) ) {
			$group = 'default';
		}
		$id  = $this->id( $key, $group );
		$gid = $this->gid( $group );
		if ( false !== $gid ) {
			$value = wp_rust_cache_incr( $gid, $this->blog( $group ), $key, $offset );
			if ( false !== $value ) {
				$this->cache[ $group ][ $id ] = $value;
				return $value;
			}
		}
		if ( ! $this->local_exists( $id, $group ) ) {
			return false;
		}
		$value = $this->cache[ $group ][ $id ];
		if ( ! is_numeric( $value ) ) {
			$value = 0;
		}
		$value += $offset;
		if ( $value < 0 ) {
			$value = 0;
		}
		$this->cache[ $group ][ $id ] = $value;
		if ( false !== $gid ) {
			// Known here but evicted from shared memory: put it back.
			wp_rust_cache_set( $gid, $this->blog( $group ), $key, $value );
		}
		return $value;
	}

	public function flush() {
		$this->cache = array();
		if ( $this->persistent ) {
			return wp_rust_cache_flush_namespace( $this->namespace );
		}
		return true;
	}

	public function flush_group( $group ) {
		if ( empty( $group ) ) {
			$group = 'default';
		}
		unset( $this->cache[ $group ] );
		$gid = $this->gid( $group );
		if ( false !== $gid ) {
			return wp_rust_cache_flush_group( $gid );
		}
		return true;
	}

	public function flush_runtime() {
		$this->cache = array();
		return true;
	}

	public function add_global_groups( $groups ) {
		$groups              = (array) $groups;
		$this->global_groups = array_merge( $this->global_groups, array_fill_keys( $groups, true ) );
	}

	public function add_non_persistent_groups( $groups ) {
		$groups                      = (array) $groups;
		$this->non_persistent_groups = array_merge( $this->non_persistent_groups, array_fill_keys( $groups, true ) );
	}

	public function switch_to_blog( $blog_id ) {
		$blog_id           = (int) $blog_id;
		$this->blog_id     = $this->multisite ? $blog_id : 0;
		$this->blog_prefix = $this->multisite ? $blog_id . ':' : '';
	}

	/** @deprecated 3.5.0 Use switch_to_blog(). */
	public function reset() {
		_deprecated_function( __FUNCTION__, '3.5.0', 'WP_Object_Cache::switch_to_blog()' );
		foreach ( array_keys( $this->cache ) as $group ) {
			if ( ! isset( $this->global_groups[ $group ] ) ) {
				unset( $this->cache[ $group ] );
			}
		}
	}

	public function stats() {
		echo '<p>';
		echo "<strong>Cache Hits:</strong> {$this->cache_hits}<br />";
		echo "<strong>Cache Misses:</strong> {$this->cache_misses}<br />";
		echo '<strong>Backend:</strong> ' . ( $this->persistent ? 'wp-rust-cache (shared memory)' : 'non-persistent' ) . '<br />';
		echo '</p>';
		echo '<ul>';
		foreach ( $this->cache as $group => $cache ) {
			echo '<li><strong>Group:</strong> ' . esc_html( $group ) . ' - ( ' . number_format( strlen( serialize( $cache ) ) / KB_IN_BYTES, 2 ) . 'k )</li>';
		}
		echo '</ul>';
	}

	public function close() {
		return true;
	}
}

/**
 * Site Health (Tools → Site Health): says whether the shared-memory cache is
 * in use and, if not, why — the site keeps working without it, so this is
 * where an administrator notices.
 */
function wp_rust_cache_site_health_test() {
	global $wp_object_cache;
	$result = array(
		'label'       => 'The wp-rust-cache object cache is in use',
		'status'      => 'good',
		'badge'       => array(
			'label' => 'Performance',
			'color' => 'blue',
		),
		'description' => '',
		'actions'     => '',
		'test'        => 'wp_rust_cache',
	);
	if ( $wp_object_cache instanceof WP_Object_Cache && $wp_object_cache->is_persistent() && function_exists( 'wp_rust_cache_stats' ) ) {
		$s     = wp_rust_cache_stats();
		$total = $s['hits'] + $s['misses'];
		$result['description'] = sprintf(
			'<p>Shared memory %1$s: %2$s of %3$s used, %4$s entries, hit ratio %5$s%%, %6$s evictions.</p>',
			esc_html( $s['path'] ),
			esc_html( size_format( $s['alloc_bytes'] ) ),
			esc_html( size_format( $s['heap_bytes'] ) ),
			number_format_i18n( $s['entries'] ),
			$total ? number_format_i18n( 100 * $s['hits'] / $total, 2 ) : '0',
			number_format_i18n( $s['evictions'] )
		);
		if ( $s['evictions'] > 0 && $s['alloc_bytes'] > 0.9 * $s['heap_bytes'] ) {
			$result['status']      = 'recommended';
			$result['label']       = 'The wp-rust-cache object cache is full';
			$result['description'] .= '<p>Entries are being evicted to make room. Raising <code>memory</code> in the wp-rust-cache configuration would keep more of the site in memory.</p>';
		}
		return $result;
	}
	$result['status'] = 'critical';
	$result['label']  = 'The wp-rust-cache object cache is not in use';
	if ( ! function_exists( 'wp_rust_cache_info' ) ) {
		$why = 'The wp_rust_cache PHP extension is not loaded in this PHP.';
	} elseif ( defined( 'WP_RUST_CACHE_DISABLED' ) && WP_RUST_CACHE_DISABLED ) {
		$why = 'WP_RUST_CACHE_DISABLED is set in wp-config.php.';
	} else {
		$info = wp_rust_cache_info();
		$why  = 'The shared-memory segment is unavailable: ' . ( $info['error'] ? $info['error'] : 'disabled in the configuration' ) . '.';
	}
	$result['description'] = '<p>' . esc_html( $why ) . ' The site works, but every request rebuilds its data from the database.</p>';
	return $result;
}

if ( function_exists( 'add_filter' ) ) {
	add_filter(
		'site_status_tests',
		function ( $tests ) {
			$tests['direct']['wp_rust_cache'] = array(
				'label' => 'wp-rust-cache',
				'test'  => 'wp_rust_cache_site_health_test',
			);
			return $tests;
		}
	);
}

if ( defined( 'WP_CLI' ) && WP_CLI && class_exists( 'WP_CLI' ) ) {

	/**
	 * Inspects and manages the wp-rust-cache object cache.
	 */
	class WP_Rust_Cache_CLI {

		private static function latency( $ns ) {
			if ( ! $ns ) {
				return 'n/a';
			}
			return $ns < 1000 ? $ns . ' ns' : number_format( $ns / 1000, 1 ) . ' µs';
		}

		private static function bytes( $b ) {
			return size_format( $b, 2 ) ?: '0 B';
		}

		private static function stats_or_fail() {
			if ( ! function_exists( 'wp_rust_cache_stats' ) ) {
				WP_CLI::error( 'The wp_rust_cache PHP extension is not loaded in this PHP (' . PHP_BINARY . ').' );
			}
			$stats = wp_rust_cache_stats();
			if ( false === $stats ) {
				$info = wp_rust_cache_info();
				WP_CLI::error( 'Shared memory is not available: ' . ( $info['error'] ?: 'disabled' ) );
			}
			return $stats;
		}

		/**
		 * Shows whether the cache is running and how it is doing.
		 *
		 * ## EXAMPLES
		 *
		 *     wp rust-cache status
		 */
		public function status( $args, $assoc_args ) {
			global $wp_object_cache;
			$s     = self::stats_or_fail();
			$ratio = $s['hits'] + $s['misses'] ? 100 * $s['hits'] / ( $s['hits'] + $s['misses'] ) : 0;
			$rows  = array(
				'Status'     => $wp_object_cache->is_persistent() ? 'RUNNING' : 'NOT IN USE BY THIS SITE',
				'Backend'    => 'shared-memory (' . $s['path'] . ')',
				'Namespace'  => $wp_object_cache->get_namespace(),
				'Memory'     => self::bytes( $s['alloc_bytes'] ) . ' / ' . self::bytes( $s['heap_bytes'] ),
				'Entries'    => number_format( $s['entries'] ),
				'Hit ratio'  => number_format( $ratio, 2 ) . '%',
				'Evictions'  => number_format( $s['evictions'] ),
				'P50 (get)'  => self::latency( $s['get_p50'] ),
				'P95 (get)'  => self::latency( $s['get_p95'] ),
				'P99 (get)'  => self::latency( $s['get_p99'] ),
			);
			WP_CLI::line( 'WP Rust Cache' );
			WP_CLI::line( '' );
			foreach ( $rows as $k => $v ) {
				WP_CLI::line( str_pad( $k . ':', 14 ) . $v );
			}
		}

		/**
		 * Prints every counter of the segment.
		 *
		 * ## OPTIONS
		 *
		 * [--format=<format>]
		 * : table or json.
		 * ---
		 * default: table
		 * ---
		 */
		public function stats( $args, $assoc_args ) {
			$s = self::stats_or_fail();
			if ( 'json' === \WP_CLI\Utils\get_flag_value( $assoc_args, 'format', 'table' ) ) {
				WP_CLI::line( wp_json_encode( $s, JSON_PRETTY_PRINT ) );
				return;
			}
			$items = array();
			foreach ( $s as $k => $v ) {
				$items[] = array(
					'metric' => $k,
					'value'  => is_float( $v ) ? number_format( $v * 100, 2 ) . '%' : $v,
				);
			}
			WP_CLI\Utils\format_items( 'table', $items, array( 'metric', 'value' ) );
		}

		/**
		 * Empties this site's cache (all blogs of a multisite).
		 *
		 * ## OPTIONS
		 *
		 * [--all]
		 * : Empty the whole segment, every site that shares it.
		 */
		public function flush( $args, $assoc_args ) {
			self::stats_or_fail();
			if ( \WP_CLI\Utils\get_flag_value( $assoc_args, 'all', false ) ) {
				wp_rust_cache_flush_all() ? WP_CLI::success( 'Segment emptied.' ) : WP_CLI::error( 'Flush failed.' );
				return;
			}
			wp_cache_flush() ? WP_CLI::success( 'Cache flushed for this site.' ) : WP_CLI::error( 'Flush failed.' );
		}

		/**
		 * Shows the value stored in shared memory for a key.
		 *
		 * ## OPTIONS
		 *
		 * <key>
		 * : Cache key.
		 *
		 * [--group=<group>]
		 * : Cache group.
		 * ---
		 * default: default
		 * ---
		 */
		public function inspect( $args, $assoc_args ) {
			self::stats_or_fail();
			global $wp_object_cache;
			$group = \WP_CLI\Utils\get_flag_value( $assoc_args, 'group', 'default' );
			$found = false;
			$value = $wp_object_cache->get( $args[0], $group, true, $found );
			if ( ! $found ) {
				WP_CLI::error( "Not in the cache: {$group}/{$args[0]}" );
			}
			WP_CLI::line( 'Type:  ' . gettype( $value ) . ( is_object( $value ) ? ' (' . get_class( $value ) . ')' : '' ) );
			WP_CLI::line( 'Value: ' . var_export( $value, true ) );
		}
	}

	WP_CLI::add_command( 'rust-cache', 'WP_Rust_Cache_CLI' );
}
