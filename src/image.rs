#![allow(unused_mut)]
#![allow(unused_imports)]
#![allow(unused_variables)]
#![allow(dead_code)]
use std::cell::RefCell;
use neon::{prelude::*, types::buffer::TypedArray};
use skia_safe::{Image as SkImage, ImageInfo, Size, ColorType, AlphaType, Data};

use crate::utils::*;


pub type BoxedImage = JsBox<RefCell<Image>>;
impl Finalize for Image {}

pub struct Image{
  src:String,
  pub image:Option<SkImage>
}

impl Image{
  pub fn info(width:f32, height:f32) -> ImageInfo {
    let dims = (width as i32, height as i32);
    ImageInfo::new(dims, ColorType::RGBA8888, AlphaType::Unpremul, None)
  }

  pub fn size(&self) -> Size{
    if let Some(img) = &self.image {
      let width = &img.width();
      let height = &img.height();
      Size::new(*width as f32, *height as f32)
    }else{
      Size::new(0.0, 0.0)
    }
  }
}

//
// -- Javascript Methods --------------------------------------------------------------------------
//

pub fn new(mut cx: FunctionContext) -> JsResult<BoxedImage> {
  let this = RefCell::new(Image{ src:"".to_string(), image:None });
  Ok(cx.boxed(this))
}

pub fn get_src(mut cx: FunctionContext) -> JsResult<JsString> {
  let this = cx.argument::<BoxedImage>(0)?;
  let this = this.borrow();

  Ok(cx.string(&this.src))
}

pub fn set_src(mut cx: FunctionContext) -> JsResult<JsUndefined> {
  let this = cx.argument::<BoxedImage>(0)?;
  let mut this = this.borrow_mut();

  let src = cx.argument::<JsString>(1)?.value(&mut cx);
  this.src = src;
  Ok(cx.undefined())
}

pub fn set_data(mut cx: FunctionContext) -> JsResult<JsBoolean> {
  let this = cx.argument::<BoxedImage>(0)?;
  let mut this = this.borrow_mut();

  let buffer = cx.argument::<JsBuffer>(1)?;
  let data = Data::new_copy(buffer.as_slice(&mut cx));

  this.image = SkImage::from_encoded(data);
  Ok(cx.boolean(this.image.is_some()))
}

pub fn load_pixel_data(mut cx: FunctionContext) -> JsResult<JsBoolean> {
  let this = cx.argument::<BoxedImage>(0)?;
  let mut this = this.borrow_mut();

  let buffer = cx.argument::<JsBuffer>(1)?;
  let data = Data::new_copy(buffer.as_slice(&mut cx));

  let image_parameters = cx.argument::<JsObject>(2)?;
  let js_width: Handle<JsNumber> = image_parameters.get(&mut cx, "width")?;
  let js_height: Handle<JsNumber> = image_parameters.get(&mut cx, "height")?;
  let js_color_type: Handle<JsString> = image_parameters.get(&mut cx, "colorType")?;

  let color_type = map_color_type(js_color_type.value(&mut cx).as_str());
  let width = js_width.value(&mut cx) as i32;
  let height = js_height.value(&mut cx) as i32;
  let row_bytes = (width as usize) * color_type.bytes_per_pixel();

  let image_info = ImageInfo::new((width, height), color_type, AlphaType::Unpremul, None);
  this.image = SkImage::from_raster_data(&image_info, data, row_bytes);

  Ok(cx.boolean(this.image.is_some()))
}

pub fn get_width(mut cx: FunctionContext) -> JsResult<JsValue> {
  let this = cx.argument::<BoxedImage>(0)?;
  let this = this.borrow();

  match &this.image {
    Some(image) => Ok(cx.number(image.width() as f64).upcast()),
    None => Ok(cx.undefined().upcast())
  }
}

pub fn get_height(mut cx: FunctionContext) -> JsResult<JsValue> {
  let this = cx.argument::<BoxedImage>(0)?;
  let this = this.borrow();

  match &this.image {
    Some(image) => Ok(cx.number(image.height() as f64).upcast()),
    None => Ok(cx.undefined().upcast())
  }
}

pub fn get_complete(mut cx: FunctionContext) -> JsResult<JsBoolean> {
  let this = cx.argument::<BoxedImage>(0)?;
  let this = this.borrow();
  Ok(cx.boolean(this.image.is_some()))
}

fn map_color_type(color_type: &str) -> ColorType {
  match color_type {
    "rgba" => ColorType::RGBA8888,
    "rgb" => ColorType::RGB888x,
    "bgra" => ColorType::BGRA8888,
    "argb" => ColorType::ARGB4444,
    _ => ColorType::RGBA8888,
  }
}
