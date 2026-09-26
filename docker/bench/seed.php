<?php
/**
 * Content for the benchmark: terms and meta on posts, 1000 WooCommerce
 * products in 20 categories, an Elementor page, and the URL list that the
 * HTTP benchmark requests.
 */

mt_srand( 42 );

$cats = get_terms( array( 'taxonomy' => 'category', 'hide_empty' => false, 'fields' => 'ids' ) );
$tags = get_terms( array( 'taxonomy' => 'post_tag', 'hide_empty' => false, 'fields' => 'ids' ) );
$posts = get_posts( array( 'numberposts' => -1, 'post_type' => 'post', 'fields' => 'ids' ) );
foreach ( $posts as $id ) {
	wp_set_post_terms( $id, array( $cats[ mt_rand( 0, count( $cats ) - 1 ) ] ), 'category' );
	wp_set_post_terms( $id, array_map( fn() => $tags[ mt_rand( 0, count( $tags ) - 1 ) ], range( 1, 4 ) ), 'post_tag' );
	for ( $m = 0; $m < 5; $m++ ) {
		update_post_meta( $id, "bench_meta_$m", str_repeat( 'v', mt_rand( 20, 400 ) ) );
	}
}

$product_cats = array();
for ( $i = 0; $i < 20; $i++ ) {
	$t              = wp_insert_term( "Category $i", 'product_cat' );
	$product_cats[] = $t['term_id'];
}
for ( $i = 0; $i < 1000; $i++ ) {
	$p = new WC_Product_Simple();
	$p->set_name( "Product $i" );
	$p->set_regular_price( (string) mt_rand( 5, 500 ) );
	$p->set_sku( "SKU-$i" );
	$p->set_manage_stock( true );
	$p->set_stock_quantity( mt_rand( 0, 100 ) );
	$p->set_description( str_repeat( "Product $i description. ", mt_rand( 5, 40 ) ) );
	$p->set_category_ids( array( $product_cats[ $i % 20 ] ) );
	$p->save();
}

// An Elementor page: heading, text, and a posts grid pulled from the cache.
$page = wp_insert_post( array( 'post_type' => 'page', 'post_status' => 'publish', 'post_title' => 'Elementor page' ) );
$data = array(
	array(
		'id'       => 'a1',
		'elType'   => 'section',
		'settings' => array(),
		'elements' => array(
			array(
				'id'       => 'a2',
				'elType'   => 'column',
				'settings' => array( '_column_size' => 100 ),
				'elements' => array(
					array( 'id' => 'a3', 'elType' => 'widget', 'widgetType' => 'heading', 'settings' => array( 'title' => 'Built with Elementor' ) ),
					array( 'id' => 'a4', 'elType' => 'widget', 'widgetType' => 'text-editor', 'settings' => array( 'editor' => '<p>Rendered through the object cache.</p>' ) ),
				),
			),
		),
	),
);
update_post_meta( $page, '_elementor_edit_mode', 'builder' );
update_post_meta( $page, '_elementor_template_type', 'wp-page' );
update_post_meta( $page, '_elementor_version', defined( 'ELEMENTOR_VERSION' ) ? ELEMENTOR_VERSION : '3.0.0' );
update_post_meta( $page, '_elementor_data', wp_slash( wp_json_encode( $data ) ) );

// URLs for the HTTP benchmark: a mix a real visitor would produce.
$urls = array( '/', '/shop/', '/?s=product' );
foreach ( array_rand( array_flip( $posts ), 300 ) as $id ) {
	$urls[] = wp_make_link_relative( get_permalink( $id ) );
}
foreach ( get_posts( array( 'post_type' => 'product', 'numberposts' => 200, 'orderby' => 'rand' ) ) as $p ) {
	$urls[] = wp_make_link_relative( get_permalink( $p ) );
}
foreach ( array_slice( $cats, 0, 30 ) as $c ) {
	$urls[] = wp_make_link_relative( get_term_link( $c ) );
}
foreach ( $product_cats as $c ) {
	$urls[] = wp_make_link_relative( get_term_link( $c ) );
}
$urls[] = wp_make_link_relative( get_permalink( $page ) );
file_put_contents( '/var/www/html/bench-urls.txt', implode( "\n", $urls ) . "\n" );
echo count( $urls ) . " URLs\n";
