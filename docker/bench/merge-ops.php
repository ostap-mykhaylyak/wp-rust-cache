<?php
// Merges the per-worker histograms of bench-ops.php and prints one row.
[ , $backend, $workers, $secs, $cpu_usec ] = $argv;
$files = array_slice( $argv, 5 );

function upper( $i ) {
	if ( $i < 16 ) {
		return $i + 1;
	}
	$e   = intdiv( $i - 16, 8 ) + 4;
	$sub = ( $i - 16 ) % 8;
	return ( 1 << $e ) + ( ( $sub + 1 ) << ( $e - 3 ) );
}

function q( $h, $p ) {
	ksort( $h );
	$total = array_sum( $h );
	if ( ! $total ) {
		return '-';
	}
	$rank = max( 1, (int) ceil( $total * $p ) );
	$seen = 0;
	foreach ( $h as $b => $c ) {
		$seen += $c;
		if ( $seen >= $rank ) {
			$ns = upper( $b );
			return $ns < 1000 ? "{$ns}ns" : ( $ns < 1e6 ? sprintf( '%.1fµs', $ns / 1e3 ) : sprintf( '%.1fms', $ns / 1e6 ) );
		}
	}
}

$get = array();
$set = array();
$ops = 0;
foreach ( $files as $f ) {
	$d = json_decode( file_get_contents( $f ), true );
	foreach ( array( 'get', 'set' ) as $k ) {
		foreach ( $d[ $k ] as $b => $c ) {
			${$k}[ $b ] = ( ${$k}[ $b ] ?? 0 ) + $c;
		}
	}
	$ops += $d['ops'];
}
printf(
	"%-10s %7d %11.0f %9s %9s %9s %9s %9s %9s %8.2fµs\n",
	$backend, $workers, $ops / $secs,
	q( $get, .5 ), q( $get, .95 ), q( $get, .99 ),
	q( $set, .5 ), q( $set, .95 ), q( $set, .99 ),
	$ops ? $cpu_usec / $ops : 0
);
