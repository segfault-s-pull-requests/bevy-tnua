fn main(){
    #[cfg(all(feature = "f32", feature = "f64"))]
    compile_error!("Feature f32 and f64 are mutually exclusive and cannot be enabled together");
}