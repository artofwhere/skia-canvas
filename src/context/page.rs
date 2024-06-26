use std::fs;
use std::path::Path as FilePath;
use rayon::prelude::*;
use neon::prelude::*;
use skia_safe::{Canvas as SkCanvas,
                Path, Matrix, Rect, IRect, Size, ISize, IPoint,
                ClipOp, Data, Color, ColorSpace, ColorType,
                PictureRecorder, Picture, EncodedImageFormat, Image as SkImage,
                svg::{self, canvas::Flags}, pdf, Document, ImageInfo, RoundOut,
                image::BitDepth, image::CachingHint};

use crc::{Crc, CRC_32_ISO_HDLC};
const CRC32: Crc<u32> = Crc::<u32>::new(&CRC_32_ISO_HDLC);

use crate::canvas::BoxedCanvas;
use crate::context::BoxedContext2D;
use crate::gpu::RenderingEngine;
use crate::utils::make_raw_image_info;

//
// Deferred canvas (records drawing commands for later replay on an output surface)
//

pub struct PageRecorder{
  current: PictureRecorder,
  layers: Vec<Picture>,
  cache: Option<SkImage>,
  bounds: Rect,
  matrix: Matrix,
  clip: Option<Path>,
  changed: bool,
}

impl PageRecorder{
  pub fn new(bounds:Rect) -> Self {
    let mut rec = PictureRecorder::new();
    rec.begin_recording(bounds, None);
    rec.recording_canvas().unwrap().save(); // start at depth 2
    PageRecorder{ current:rec, changed:false, layers:vec![], cache:None, matrix:Matrix::default(), clip:None, bounds }
  }

  pub fn append<F>(&mut self, f:F)
    where F:FnOnce(&mut SkCanvas)
  {
    if let Some(canvas) = self.current.recording_canvas() {
      f(canvas);
      self.changed = true;
    }
  }

  pub fn set_bounds(&mut self, bounds:Rect){
    *self = PageRecorder::new(bounds);
  }

  pub fn update_bounds(&mut self, bounds:Rect){
    self.bounds = bounds; // non-destructively update the size
  }

  pub fn set_matrix(&mut self, matrix:Matrix){
    self.matrix = matrix;
    if let Some(canvas) = self.current.recording_canvas() {
      canvas.set_matrix(&matrix.into());
    }
  }

  pub fn set_clip(&mut self, clip:&Option<Path>){
    self.clip = clip.clone();
    self.restore();
  }

  pub fn restore(&mut self){
    if let Some(canvas) = self.current.recording_canvas() {
      canvas.restore_to_count(1);
      canvas.save();
      if let Some(clip) = &self.clip{
        canvas.clip_path(&clip, ClipOp::Intersect, true /* antialias */);
      }
      canvas.set_matrix(&self.matrix.into());
    }
  }

  pub fn get_page(&mut self) -> Page{
    if self.changed {
      // stop and restart the recorder while adding its content as a new layer
      if let Some(palimpsest) = self.current.finish_recording_as_picture(Some(&self.bounds)) {
        self.layers.push(palimpsest);
      }
      self.current.begin_recording(self.bounds, None);
      self.changed = false;
      self.cache = None;
      self.restore();
    }

    Page{
      layers: self.layers.clone(),
      bounds: self.bounds,
    }
  }

  pub fn get_image(&mut self) -> Option<SkImage>{
    let page = self.get_page();
    if self.cache.is_none(){
      if let Some(pict) = page.get_picture(None, None){
        let size = page.bounds.size().to_floor();
        self.cache = SkImage::from_picture(pict, size, None, None, BitDepth::U8, Some(ColorSpace::new_srgb()));
      }
    }
    self.cache.clone()
  }
}

//
// Image generator for a single drawing context
//

#[derive(Debug, Clone)]
pub struct Page{
  pub layers: Vec<Picture>,
  pub bounds: Rect,
}

impl Page{

  pub fn get_picture(&self, matte:Option<Color>, bounds:Option<&Rect>) -> Option<Picture> {
    let mut compositor = PictureRecorder::new();
    let bounds = bounds.unwrap_or(&self.bounds);
    compositor.begin_recording(bounds, None);
    if let Some(output) = compositor.recording_canvas() {
      matte.map(|c| output.clear(c));
      for pict in self.layers.iter(){
        pict.playback(output);
      }
    }
    compositor.finish_recording_as_picture(Some(bounds))
  }

  pub fn get_image(&self, picture: &Picture, color_space: impl Into<Option<ColorSpace>>, bit_depth: Option<BitDepth>) -> Result<SkImage, String> {
    SkImage::from_picture(picture, self.bounds.size().to_floor(), None, None, bit_depth.unwrap_or(BitDepth::U8), color_space)
    .ok_or("Error generating image".to_string())
  }

  pub fn get_pixels(&self, buffer: &mut [u8], picture: &Picture, info: &ImageInfo/* , origin: impl Into<IPoint> */) -> bool {
    if let Ok(img) = self.get_image(picture, info.color_space(), None) {
      let bounds: IRect = picture.cull_rect().round_in();
      // println!("get_pixels() bounds {:?}; imgBounds: {:?}; infoBounds: {:?}", bounds, img.bounds(), info.bounds());
      return img.read_pixels(&info, buffer, info.min_row_bytes(), (bounds.left, bounds.top), CachingHint::Allow);
    }
    false
  }

  #[allow(clippy::too_many_arguments)]
  pub fn encoded_as(&self,
      format:&str,
      quality:f32,
      density:f32,
      outline:bool,
      matte:Option<Color>,
      bounds: Option<Rect>,           // clip area, for raster or raw
      premultiplied:Option<bool>,     // for raw
      color_type: Option<ColorType>,  // for raw
      engine:RenderingEngine
  ) -> Result<Data, String> {
    let render_bounds = bounds.unwrap_or(self.bounds);
    if render_bounds.is_empty() || !self.bounds.intersects(render_bounds) {
      return Err(
        format!("Render bounds ({:?}) must be non-empty and intersect the canvas bounds ({:?}).", render_bounds, self.bounds)
      );
    }
    let picture = self.get_picture(matte, Some(&render_bounds)).ok_or("Could not generate picture")?;

    let img_format = match format {
      "jpg" | "jpeg" => Some(EncodedImageFormat::JPEG),
      "png" => Some(EncodedImageFormat::PNG),
      _ => None
    };

    if let Some(img_format) = img_format {
      let img_scale = Matrix::scale((density, density));
      // start with full `self.bounds` size since image_snapshot_with_bounds() will crop as needed
      let img_dims = Size::new(self.bounds.width() * density, self.bounds.height() * density).to_floor();
      let img_info = ImageInfo::new_n32_premul(img_dims, Some(ColorSpace::new_srgb()));
      // let img_info = make_raw_image_info(img_dims, premultiplied, color_type);  // let surface dictate color type?

      if let Some(mut surface) = engine.get_surface(&img_info){
        surface
          .canvas()
          .set_matrix(&img_scale.into())
          .draw_picture(&picture, None, None);
        // This unwrap() should never panic because we validated the render bounds already.
        surface.image_snapshot_with_bounds(&render_bounds.round_in()).unwrap()
          .encode_to_data_with_quality(img_format, (quality*100.0) as i32)
          .map(|data| with_dpi(data, img_format, density))
          .ok_or(format!("Could not encode as {}", format))
      }else{
        Err(format!("Could not allocate new {}×{} bitmap", img_dims.width, img_dims.height))
      }
    }
    else if format == "pdf" {
      let mut document = pdf_document(quality, density).begin_page(self.bounds.size().to_floor(), None);
      let canvas = document.canvas();
      canvas.draw_picture(&picture, None, None);
      Ok(document.end_page().close())
    }
    else if format == "svg" {
      let flags = outline.then(|| Flags::CONVERT_TEXT_TO_PATHS);
      let mut canvas = svg::Canvas::new(Rect::from_size(self.bounds.size()), flags);
      canvas.draw_picture(&picture, None, None);
      Ok(canvas.end())
    }
    else if format == "raw" {
      let img_dims = render_bounds.size().to_floor();
      let info = make_raw_image_info(img_dims, premultiplied, color_type);
      let mut buffer: Vec<u8> = vec![0; info.compute_min_byte_size()];
      if self.get_pixels(&mut buffer, &picture, &info /*, (bounds.left.floor() as i32, bounds.top.floor() as i32) */) {
        return Ok(Data::new_copy(&buffer));
      }
      return Err("Could not encode as raw bytes".to_string());
    }
    else {
      Err(format!("Unsupported file format {}", format))
    }
  }

  #[allow(clippy::too_many_arguments)]
  pub fn write(&self,
      filename: &str,
      file_format:&str,
      quality:f32,
      density:f32,
      outline:bool,
      matte:Option<Color>,
      bounds: Option<Rect>,
      premultiplied:Option<bool>,
      color_type: Option<ColorType>,
      engine:RenderingEngine
  ) -> Result<(), String> {
    let path = FilePath::new(&filename);
    let data = self.encoded_as(file_format, quality, density, outline, matte, bounds, premultiplied, color_type, engine)?;
    fs::write(path, data.as_bytes()).map_err(|why|
      format!("{}: \"{}\"", why, path.display())
    )
  }

  fn append_to(&self, doc:Document, matte:Option<Color>) -> Result<Document, String>{
    if !self.bounds.is_empty(){
      let mut doc = doc.begin_page(self.bounds.size(), None);
      let canvas = doc.canvas();
      if let Some(picture) = self.get_picture(matte, None){
        canvas.draw_picture(&picture, None, None);
      }
      Ok(doc.end_page())
    }else{
      Err("Width and height must be non-zero to generate a PDF page".to_string())
    }
  }
}


//
// Container for a canvas's entire stack of page contexts
//

pub struct PageSequence{
  pub pages: Vec<Page>,
  pub engine: RenderingEngine
}

impl PageSequence{
  pub fn from(pages:Vec<Page>, engine:RenderingEngine) -> Self{
    PageSequence { pages, engine }
  }

  pub fn first(&self) -> &Page {
    &self.pages[0]
  }

  pub fn len(&self) -> usize{
    self.pages.len()
  }

  pub fn as_pdf(&self, quality:f32, density:f32, matte:Option<Color>) -> Result<Data, String>{
    self.pages
      .iter()
      .try_fold(pdf_document(quality, density), |doc, page| page.append_to(doc, matte))
      .map(|doc| doc.close())
  }

  #[allow(clippy::too_many_arguments)]
  pub fn write_image(&self,
      pattern:&str,
      format:&str,
      quality:f32,
      density:f32,
      outline:bool,
      matte:Option<Color>,
      bounds: Option<Rect>,
      premultiplied:Option<bool>,
      color_type: Option<ColorType>
    ) -> Result<(), String>{
    self.first().write(&pattern, &format, quality, density, outline, matte, bounds, premultiplied, color_type, self.engine)
  }

  #[allow(clippy::too_many_arguments)]
  pub fn write_sequence(&self,
      pattern:&str,
      format:&str,
      padding:f32,
      quality:f32,
      density:f32,
      outline:bool,
      matte:Option<Color>,
      bounds: Option<Rect>,
      premultiplied:Option<bool>,
      color_type: Option<ColorType>
  ) -> Result<(), String>{
    let padding = match padding as i32{
      -1 => (1.0 + (self.pages.len() as f32).log10().floor()) as usize,
      pad => pad as usize
    };

    self.pages
      .par_iter()
      .enumerate()
      .try_for_each(|(pp, page)|{
        let folio = format!("{:0width$}", pp+1, width=padding);
        let filename = pattern.replace("{}", folio.as_str());
        page.write(&filename, format, quality, density, outline, matte, bounds, premultiplied, color_type, self.engine)
      })
  }

  pub fn write_pdf(&self, path:&str, quality:f32, density:f32, matte:Option<Color>) -> Result<(), String>{
    let path = FilePath::new(&path);
    match self.as_pdf(quality, density, matte){
      Ok(document) => fs::write(path, document.as_bytes()).map_err(|why|
        format!("{}: \"{}\"", why, path.display())
      ),
      Err(msg) => Err(msg)
    }
  }

}

//
// Helpers
//

pub fn pages_arg(cx: &mut FunctionContext, idx: i32, canvas:&BoxedCanvas) -> NeonResult<PageSequence> {
  let engine = canvas.borrow().engine;
  let pages = cx.argument::<JsArray>(idx)?
      .to_vec(cx)?
      .iter()
      .map(|obj| obj.downcast::<BoxedContext2D, _>(cx))
      .filter( |ctx| ctx.is_ok() )
      .map(|obj| obj.unwrap().borrow().get_page())
      .collect();
  Ok(PageSequence::from(pages, engine))
}

fn pdf_document(quality:f32, density:f32) -> Document{
  let mut meta = pdf::Metadata::default();
  meta.producer = "Skia Canvas <https://github.com/samizdatco/skia-canvas>".to_string();
  meta.encoding_quality = Some((quality*100.0) as i32);
  meta.raster_dpi = Some(density * 72.0);
  pdf::new_document(Some(&meta))
}

fn with_dpi(data:Data, format:EncodedImageFormat, density:f32) -> Data{
  if density as u32 == 1 { return data }

  let mut bytes = data.as_bytes().to_vec();
  match format{
    EncodedImageFormat::JPEG => {
      let [l, r] = (72 * density as u16).to_be_bytes();
      bytes.splice(13..18, [1, l, r, l, r].iter().cloned());
      Data::new_copy(&bytes)
    }
    EncodedImageFormat::PNG => {
      let mut digest = CRC32.digest();
      let [a, b, c, d] = ((72.0 * density * 39.3701) as u32).to_be_bytes();
      let phys = vec![
        b'p', b'H', b'Y', b's',
        a, b, c, d, // x-dpi
        a, b, c, d, // y-dpi
        1, // dots per meter
      ];
      digest.update(&phys);

      let length = 9u32.to_be_bytes().to_vec();
      let checksum = digest.finalize().to_be_bytes().to_vec();
      bytes.splice(33..33, [length, phys, checksum].concat());
      Data::new_copy(&bytes)
    }
    _ => data
  }
}
